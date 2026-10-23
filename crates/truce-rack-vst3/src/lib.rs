//! VST3 host implementation for the truce-rack framework.
//!
//! Built on the community `vst3` Rust bindings — no Steinberg SDK
//! submodule, no cmake. A fresh checkout builds in seconds.
//!
//! # Lifecycle
//!
//! Each `.vst3` bundle exports `GetPluginFactory` (and on macOS
//! `bundleEntry` / `bundleExit`). Loading walks:
//!
//! 1. `dlopen` the binary inside the bundle.
//! 2. `bundleEntry()` on macOS (no-op elsewhere).
//! 3. `GetPluginFactory()` → `IPluginFactory`.
//! 4. `createInstance(cid, IComponent::IID)` → `IComponent`.
//! 5. `IPluginBase::initialize(host_context)` on the component.
//! 6. Cast to `IAudioProcessor`.
//! 7. `IComponent::getControllerClassId(&mut cid2)` — if separate,
//!    `factory.createInstance(cid2, IEditController::IID)` and
//!    initialize it. Otherwise cast the component itself.
//! 8. Connect the component and controller `IConnectionPoint`s
//!    (separate-controller case only).
//!
//! Activate calls `setBusArrangements`, `setupProcessing`,
//! `setActive(true)`, `setProcessing(true)`. Deactivate reverses.

use truce_rack_core::buffer::AudioBuffer;
use truce_rack_core::bus::BusLayout;
use truce_rack_core::controller::{ChangeKind, ChangeListener, ParamChange, PluginController};
use truce_rack_core::error::{Error, Result};
use truce_rack_core::events::EventList;
use truce_rack_core::info::{ParameterInfo, PluginCategory, PluginInfo, PresetInfo};
use truce_rack_core::plugin::{Plugin, PluginCore, ProcessContext, ProcessStatus};
use truce_rack_core::scanner::PluginScanner;
use truce_rack_core::transport::TransportInfo;
use truce_rack_core::wrapper::run_audio_block_with;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, Event, Event_::EventTypes_, Event__type0,
    IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler, IComponentHandlerTrait,
    IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
    IEditControllerTrait, IEventList, IEventListTrait, IHostApplication, IHostApplicationTrait,
    IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait,
    NoteOffEvent, NoteOnEvent, ParamID, ParamValue, ParameterInfo as Vst3ParameterInfo,
    ParameterInfo_::ParameterFlags_, PolyPressureEvent, ProcessContext as Vst3ProcessContext,
    ProcessContext_::StatesAndFlags_, ProcessData, ProcessModes_, ProcessSetup, String128,
    SymbolicSampleSizes_, ViewType,
};
use vst3::Steinberg::{
    FUnknown, IBStream, IBStreamTrait, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait,
    IPluginBaseTrait, IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait,
    PClassInfo, PClassInfo_, PClassInfo2, PFactoryInfo, TUID, ViewRect, kPlatformTypeHWND,
    kPlatformTypeNSView, kPlatformTypeX11EmbedWindowID, kResultFalse, kResultOk, kResultTrue,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper};

/// Format identifier used on returned [`PluginInfo`].
pub const FORMAT: &str = "vst3";

/// Bundle directory suffix every VST3 plugin uses.
pub const VST3_EXTENSION: &str = ".vst3";

/// Symbol name `GetPluginFactory` plugins export. Same on every OS.
const GET_FACTORY_SYMBOL: &[u8] = b"GetPluginFactory\0";

/// macOS-only bundle entry point.
#[cfg(target_os = "macos")]
const BUNDLE_ENTRY_SYMBOL: &[u8] = b"bundleEntry\0";

/// macOS-only bundle exit point.
#[cfg(target_os = "macos")]
const BUNDLE_EXIT_SYMBOL: &[u8] = b"bundleExit\0";

/// Stereo speaker arrangement = `kSpeakerL | kSpeakerR`. Defined
/// here to avoid a `kSpeaker*` import dance.
const STEREO_ARRANGEMENT: u64 = 0x03;

/// VST3 scanner.
#[derive(Debug, Default)]
pub struct Vst3Scanner;

impl Vst3Scanner {
    /// Construct a default scanner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl PluginScanner for Vst3Scanner {
    type Plugin = Vst3Plugin;

    fn scan(&self) -> Result<Vec<PluginInfo>> {
        let mut out = Vec::new();
        for dir in default_vst3_paths() {
            if dir.exists() {
                scan_dir_into(&dir, &mut out);
            }
        }
        Ok(out)
    }

    fn scan_path(&self, path: &Path) -> Result<Vec<PluginInfo>> {
        let mut out = Vec::new();
        if path.exists() {
            scan_dir_into(path, &mut out);
        }
        Ok(out)
    }

    fn load(&self, info: &PluginInfo) -> Result<Self::Plugin> {
        Vst3Plugin::load_from(info)
    }
}

/// Standard VST3 install locations for the current OS.
#[must_use]
pub fn default_vst3_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let mut user = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        user.push("Library/Audio/Plug-Ins/VST3");
        #[cfg(target_os = "linux")]
        user.push(".vst3");
        #[cfg(target_os = "windows")]
        user.push("AppData/Local/Programs/Common/VST3");
        out.push(user);
    }
    #[cfg(target_os = "macos")]
    out.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
    #[cfg(target_os = "linux")]
    out.push(PathBuf::from("/usr/lib/vst3"));
    #[cfg(target_os = "windows")]
    {
        if let Some(pf) = std::env::var_os("CommonProgramFiles") {
            let mut p = PathBuf::from(pf);
            p.push("VST3");
            out.push(p);
        }
    }
    out
}

fn scan_dir_into(dir: &Path, out: &mut Vec<PluginInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.ends_with(VST3_EXTENSION) {
            continue;
        }
        if let Err(err) = scan_bundle_into(&path, out) {
            eprintln!("[truce-rack-vst3] skipping {}: {err}", path.display());
        }
    }
}

fn scan_bundle_into(bundle: &Path, out: &mut Vec<PluginInfo>) -> Result<()> {
    let module = unsafe { LoadedModule::open(bundle) }?;
    let factory = module.factory()?;
    // IPluginFactory2 adds sub-categories ("Instrument|Synth") and vendor.
    let factory2 = factory.as_com_ref().cast::<IPluginFactory2>();
    let mut factory_info: PFactoryInfo = unsafe { std::mem::zeroed() };
    let factory_vendor = if unsafe { factory.getFactoryInfo(&raw mut factory_info) } == kResultOk {
        char8_array_to_string(&factory_info.vendor)
    } else {
        String::new()
    };
    let count = unsafe { factory.countClasses() };
    let mut info = empty_pclass_info();
    for idx in 0..count {
        if unsafe { factory.getClassInfo(idx, &raw mut info) } != kResultOk {
            continue;
        }
        let category = char8_array_to_string(&info.category);
        if category != "Audio Module Class" {
            continue;
        }
        let name = char8_array_to_string(&info.name);
        let mut info2: PClassInfo2 = unsafe { std::mem::zeroed() };
        let (subcategories, vendor) = match &factory2 {
            Some(f2) if unsafe { f2.getClassInfo2(idx, &raw mut info2) } == kResultOk => (
                char8_array_to_string(&info2.subCategories),
                char8_array_to_string(&info2.vendor),
            ),
            _ => (String::new(), String::new()),
        };
        let category = category_from_subcategories(&subcategories);
        out.push(PluginInfo {
            name,
            vendor: if vendor.is_empty() {
                factory_vendor.clone()
            } else {
                vendor
            },
            version: 0,
            category,
            path: bundle.to_path_buf(),
            unique_id: tuid_to_hex(&info.cid),
            format: FORMAT,
            has_editor: false,
            accepts_midi: category == PluginCategory::Instrument,
        });
    }
    Ok(())
}

/// Maps VST3 `subCategories` ("Instrument|Synth", "Fx|Reverb", …).
fn category_from_subcategories(sub: &str) -> PluginCategory {
    let parts: Vec<&str> = sub.split('|').collect();
    if parts.contains(&"Instrument") {
        PluginCategory::Instrument
    } else if parts.contains(&"Analyzer") {
        PluginCategory::Analyzer
    } else {
        PluginCategory::Effect
    }
}

fn empty_pclass_info() -> PClassInfo {
    PClassInfo {
        cid: [0; 16],
        cardinality: 0,
        category: [0; PClassInfo_::kCategorySize as usize],
        name: [0; PClassInfo_::kNameSize as usize],
    }
}

#[allow(clippy::cast_sign_loss)]
fn char8_array_to_string(array: &[i8]) -> String {
    // VST3 char8 is signed on Apple, unsigned elsewhere; the cast
    // preserves bit pattern.
    let bytes: Vec<u8> = array
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[allow(clippy::cast_sign_loss)]
fn tuid_to_hex(cid: &[i8; 16]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(32);
    for &b in cid {
        let _ = write!(s, "{:02x}", b as u8);
    }
    s
}

#[allow(clippy::cast_possible_wrap)]
fn hex_to_tuid(hex: &str) -> Option<TUID> {
    if hex.len() != 32 {
        return None;
    }
    let mut out: TUID = [0; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()? as i8;
    }
    Some(out)
}

#[cfg(target_os = "macos")]
mod mac {
    //! `CFBundle`-backed loader for VST3 bundles on macOS.
    //!
    //! Plugins like Surge XT Effects call into `CFPlugin` during
    //! `bundleEntry` (specifically `CFPlugInRegisterFactories`,
    //! which lives behind the `AddInstanceForFactory` log line).
    //! Those APIs only work when the dylib was loaded through a
    //! registered `CFBundle` — raw `dlopen` leaves the bundle
    //! unknown to CoreFoundation and the plugin dereferences
    //! garbage. Going through `CFBundleLoadExecutable` gives the
    //! plugin the context it expects.

    use std::path::{Path, PathBuf};

    use core_foundation::base::TCFType;
    use core_foundation::bundle::CFBundle;
    use core_foundation::string::CFString;
    use core_foundation::url::{CFURL, kCFURLPOSIXPathStyle};

    use truce_rack_core::error::{Error, Result};

    pub(super) struct MacBundle {
        bundle: CFBundle,
        path: PathBuf,
    }

    impl MacBundle {
        pub(super) fn open(bundle_path: &Path) -> Result<Self> {
            let path_str = bundle_path.to_str().ok_or_else(|| Error::LoadFailed {
                path: bundle_path.to_path_buf(),
                reason: "bundle path is not valid UTF-8".into(),
            })?;
            let cf_path = CFString::new(path_str);
            // CFURLCreateWithFileSystemPath with `isDirectory = true`
            // is what every reference VST3 host uses on macOS — a
            // bundle URL has to be flagged as a directory or
            // CFBundleCreate silently picks the wrong layout.
            let url = unsafe {
                use core_foundation_sys::base::kCFAllocatorDefault;
                use core_foundation_sys::url::CFURLCreateWithFileSystemPath;
                let raw = CFURLCreateWithFileSystemPath(
                    kCFAllocatorDefault,
                    cf_path.as_concrete_TypeRef(),
                    kCFURLPOSIXPathStyle,
                    1,
                );
                if raw.is_null() {
                    return Err(Error::LoadFailed {
                        path: bundle_path.to_path_buf(),
                        reason: "CFURLCreateWithFileSystemPath returned NULL".into(),
                    });
                }
                CFURL::wrap_under_create_rule(raw)
            };

            let bundle = CFBundle::new(url).ok_or_else(|| Error::LoadFailed {
                path: bundle_path.to_path_buf(),
                reason: "CFBundleCreate returned NULL".into(),
            })?;

            // CFBundleLoadExecutable must run before any function
            // pointer lookup. Returns true on success.
            let loaded = unsafe {
                use core_foundation_sys::bundle::CFBundleLoadExecutable;
                CFBundleLoadExecutable(bundle.as_concrete_TypeRef()) != 0
            };
            if !loaded {
                return Err(Error::LoadFailed {
                    path: bundle_path.to_path_buf(),
                    reason: "CFBundleLoadExecutable failed".into(),
                });
            }

            Ok(Self {
                bundle,
                path: bundle_path.to_path_buf(),
            })
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        /// The raw `CFBundleRef`, type-erased to `*mut c_void` so
        /// the call site doesn't need to depend on
        /// `core-foundation-sys`. Hand this to `bundleEntry` —
        /// it's the host's identity to the plugin.
        pub(super) fn raw(&self) -> *mut std::ffi::c_void {
            self.bundle.as_concrete_TypeRef().cast::<std::ffi::c_void>()
        }

        /// Look up an exported symbol. `name` may end in a trailing
        /// NUL (we strip it before handing the text to `CFString`).
        pub(super) unsafe fn function_ptr(&self, name: &[u8]) -> Option<*mut std::ffi::c_void> {
            let name = match name.split_last() {
                Some((0, rest)) => rest,
                _ => name,
            };
            let name_str = std::str::from_utf8(name).ok()?;
            let cf_name = CFString::new(name_str);
            let ptr = unsafe {
                use core_foundation_sys::bundle::CFBundleGetFunctionPointerForName;
                CFBundleGetFunctionPointerForName(
                    self.bundle.as_concrete_TypeRef(),
                    cf_name.as_concrete_TypeRef(),
                )
            };
            if ptr.is_null() {
                None
            } else {
                Some(ptr.cast_mut().cast::<std::ffi::c_void>())
            }
        }
    }

    // SAFETY: CFBundle is reference-counted by CoreFoundation; we
    // hold one owned reference and CoreFoundation itself is
    // thread-safe for read access. The Drop impl releases the
    // CFBundle but deliberately never calls
    // `CFBundleUnloadExecutable` — VST3 plugins leave Objective-C
    // class registrations and runloop callbacks pointing into the
    // dylib, and unloading invalidates them. Same "don't dlclose"
    // discipline truce-loader follows on the plugin side.
    unsafe impl Send for MacBundle {}
}

/// Per-platform VST3 bundle layout. Linux uses
/// `Contents/<arch>-linux/<stem>.so`; Windows uses
/// `Contents/<arch>-win/<stem>.vst3`. macOS goes through CFBundle
/// instead (see [`mac::MacBundle`]) so its binary lookup lives
/// there.
#[cfg(not(target_os = "macos"))]
fn bundle_binary_path(bundle: &Path) -> PathBuf {
    let stem = bundle
        .file_stem()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    #[cfg(target_os = "linux")]
    {
        if bundle.is_dir() {
            let arch_dir = format!("{}-linux", std::env::consts::ARCH);
            let mut binary = stem.clone();
            binary.push(".so");
            return bundle.join("Contents").join(arch_dir).join(binary);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if bundle.is_dir() {
            let arch_dir = format!("{}-win", std::env::consts::ARCH);
            let mut binary = stem.clone();
            binary.push(".vst3");
            return bundle.join("Contents").join(arch_dir).join(binary);
        }
    }
    let _ = stem;
    bundle.to_path_buf()
}

/// RAII wrapper around the loaded module. On macOS this is a real
/// `CFBundle` so the plugin's `bundleEntry` sees the CFPlugin /
/// CFBundleGetIdentifier context it expects (raw dlopen crashes
/// some bundles — Surge XT Effects calls into CFPlugin's
/// `AddInstanceForFactory` during init). On Linux / Windows the
/// underlying file is a plain dynamic library; `libloading` is
/// enough.
#[cfg(not(target_os = "macos"))]
struct LoadedModule {
    library: libloading::Library,
}

#[cfg(target_os = "macos")]
struct LoadedModule {
    bundle: mac::MacBundle,
    entered: bool,
}

#[cfg(not(target_os = "macos"))]
impl LoadedModule {
    unsafe fn open(bundle: &Path) -> Result<Self> {
        let binary = bundle_binary_path(bundle);
        let library =
            unsafe { libloading::Library::new(&binary) }.map_err(|e| Error::LoadFailed {
                path: bundle.to_path_buf(),
                reason: format!("dlopen: {e}"),
            })?;
        Ok(Self { library })
    }

    fn factory(&self) -> Result<ComPtr<IPluginFactory>> {
        let get_factory: libloading::Symbol<'_, unsafe extern "C" fn() -> *mut IPluginFactory> =
            unsafe { self.library.get(GET_FACTORY_SYMBOL) }.map_err(|e| Error::LoadFailed {
                path: PathBuf::new(),
                reason: format!("missing GetPluginFactory: {e}"),
            })?;
        let ptr = unsafe { get_factory() };
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(ptr) }.ok_or_else(|| {
            Error::LoadFailed {
                path: PathBuf::new(),
                reason: "GetPluginFactory returned NULL".into(),
            }
        })?;
        Ok(factory)
    }
}

#[cfg(target_os = "macos")]
impl LoadedModule {
    unsafe fn open(bundle: &Path) -> Result<Self> {
        let mac_bundle = mac::MacBundle::open(bundle)?;

        // VST3 macOS spec: bundleEntry takes the CFBundleRef the
        // host loaded the plugin from. Surge XT Effects (and any
        // bundle that touches CFPlugin/AU registration in init)
        // dereferences that argument; passing nothing crashes
        // inside CFRetain on a register that happened to be
        // non-zero. Reference impl: Steinberg's `module_mac.mm`.
        let entered = unsafe {
            match mac_bundle.function_ptr(BUNDLE_ENTRY_SYMBOL) {
                Some(ptr) => {
                    let entry: unsafe extern "C" fn(*mut std::ffi::c_void) -> bool =
                        std::mem::transmute(ptr);
                    entry(mac_bundle.raw())
                }
                None => false,
            }
        };

        Ok(Self {
            bundle: mac_bundle,
            entered,
        })
    }

    fn factory(&self) -> Result<ComPtr<IPluginFactory>> {
        let raw = unsafe {
            self.bundle
                .function_ptr(GET_FACTORY_SYMBOL)
                .ok_or_else(|| Error::LoadFailed {
                    path: self.bundle.path().to_path_buf(),
                    reason: "missing GetPluginFactory".into(),
                })?
        };
        let get_factory: unsafe extern "C" fn() -> *mut IPluginFactory =
            unsafe { std::mem::transmute(raw) };
        let ptr = unsafe { get_factory() };
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(ptr) }.ok_or_else(|| {
            Error::LoadFailed {
                path: self.bundle.path().to_path_buf(),
                reason: "GetPluginFactory returned NULL".into(),
            }
        })?;
        Ok(factory)
    }
}

#[cfg(target_os = "macos")]
impl Drop for LoadedModule {
    fn drop(&mut self) {
        // bundleExit is the symmetric partner to bundleEntry and
        // takes no arguments per the VST3 macOS spec — only
        // bundleEntry sees the CFBundleRef. Most plugins are
        // no-ops; some unregister CFPlugin factories here.
        if self.entered
            && let Some(ptr) = unsafe { self.bundle.function_ptr(BUNDLE_EXIT_SYMBOL) }
        {
            let exit: unsafe extern "C" fn() -> bool = unsafe { std::mem::transmute(ptr) };
            unsafe {
                exit();
            }
        }
    }
}

/// One loaded VST3 plugin instance.
///
/// Holds three COM smart pointers — component, audio processor,
/// edit controller — plus the dlopen handle that keeps the
/// underlying dylib mapped. When `Drop`s, the COM pointers
/// release their objects which triggers `terminate()` and
/// component disposal.
pub struct Vst3Plugin {
    info: PluginInfo,
    layouts: Vec<BusLayout>,
    active_layout: Option<BusLayout>,

    // Hold the module open for the lifetime of the instance.
    _module: LoadedModule,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ComPtr<IEditController>,
    /// `true` when controller and component are *different* COM
    /// objects (separate-controller architecture) and we've wired
    /// their connection points.
    separate_controller: bool,
    component_cp: Option<ComPtr<IConnectionPoint>>,
    controller_cp: Option<ComPtr<IConnectionPoint>>,

    param_count: usize,
    processing: bool,

    /// UI / parameter side; `None` once handed out by `take_controller`.
    ctl: Option<Vst3Controller>,
    // Host-side COM objects the plugin holds pointers to; kept alive here.
    _host: ComWrapper<HostApplication>,
    _handler: ComWrapper<ComponentHandler>,
    // Pre-allocated per-block buffers so `process` doesn't allocate.
    in_events: ComWrapper<EventList3>,
    out_events: ComWrapper<EventList3>,
    in_params: ComWrapper<ParamChanges>,
}

impl Vst3Plugin {
    #[allow(clippy::too_many_lines)]
    fn load_from(info: &PluginInfo) -> Result<Self> {
        let module = unsafe { LoadedModule::open(&info.path) }?;
        let factory = module.factory()?;

        let cid = hex_to_tuid(&info.unique_id).ok_or_else(|| Error::LoadFailed {
            path: info.path.clone(),
            reason: format!("could not parse VST3 unique_id {:?}", info.unique_id),
        })?;

        // Create IComponent.
        let component_ptr =
            unsafe { create_instance::<IComponent>(&factory, &cid) }.ok_or_else(|| {
                Error::LoadFailed {
                    path: info.path.clone(),
                    reason: "factory.createInstance(IComponent) returned NULL".into(),
                }
            })?;
        let component = component_ptr;

        // Initialize the component with a host context; some plugins
        // refuse to load without IHostApplication.
        let host = ComWrapper::new(HostApplication);
        let host_ptr: *mut FUnknown = host
            .as_com_ref::<IHostApplication>()
            .map_or(ptr::null_mut(), |h| h.as_ptr().cast());
        if unsafe { component.initialize(host_ptr) } != kResultOk {
            return Err(Error::LoadFailed {
                path: info.path.clone(),
                reason: "IComponent::initialize returned non-OK".into(),
            });
        }

        // Cast to IAudioProcessor.
        let processor = component
            .as_com_ref()
            .cast::<IAudioProcessor>()
            .ok_or_else(|| Error::LoadFailed {
                path: info.path.clone(),
                reason: "component does not implement IAudioProcessor".into(),
            })?;

        // Look up the separate-controller class id; if present we
        // create a separate controller and connect to it. Otherwise
        // the component is itself the controller.
        let mut controller_cid: TUID = [0; 16];
        let cls_id_status = unsafe { component.getControllerClassId(&raw mut controller_cid) };
        let (controller, separate_controller) = if cls_id_status == kResultTrue {
            let ctrl_ptr = unsafe { create_instance::<IEditController>(&factory, &controller_cid) }
                .ok_or_else(|| Error::LoadFailed {
                    path: info.path.clone(),
                    reason: "factory.createInstance(IEditController) returned NULL".into(),
                })?;
            if unsafe { ctrl_ptr.initialize(host_ptr) } != kResultOk {
                return Err(Error::LoadFailed {
                    path: info.path.clone(),
                    reason: "IEditController::initialize returned non-OK".into(),
                });
            }
            (ctrl_ptr, true)
        } else {
            // Single-component plugin — controller and component
            // share an object. Cast through queryInterface so the
            // refcount is correct.
            let ctrl = component
                .as_com_ref()
                .cast::<IEditController>()
                .ok_or_else(|| Error::LoadFailed {
                    path: info.path.clone(),
                    reason:
                        "component is not its own controller and didn't report a controller cid"
                            .into(),
                })?;
            (ctrl, false)
        };

        // Optional: connect the two connection points so the
        // plugin's component and controller can talk to each
        // other (param/audio synchronisation). Best-effort —
        // some plugins skip these even when the controller is
        // separate.
        let (component_cp, controller_cp) = if separate_controller {
            let cp_a = component.as_com_ref().cast::<IConnectionPoint>();
            let cp_b = controller.as_com_ref().cast::<IConnectionPoint>();
            if let (Some(a), Some(b)) = (&cp_a, &cp_b) {
                unsafe {
                    a.connect(b.as_com_ref().as_ptr().cast());
                    b.connect(a.as_com_ref().as_ptr().cast());
                }
            }
            (cp_a, cp_b)
        } else {
            (None, None)
        };

        let param_count_raw = unsafe { controller.getParameterCount() }.max(0);
        #[allow(clippy::cast_sign_loss)]
        let param_count = param_count_raw as usize;

        let mut info = info.clone();
        // The editor exists if `createView("editor")` returns a
        // non-null view. We probe by creating + releasing it once
        // at load time. (Some plugins are slow to create the view;
        // a heavier-weight host would defer this to first open.)
        info.has_editor = unsafe { create_editor_view(&controller) }.is_some();

        // Edits made in the plugin's own UI arrive through this handler.
        let listener: SharedListener = Arc::default();
        let handler = ComWrapper::new(ComponentHandler {
            listener: listener.clone(),
        });
        if let Some(h) = handler.as_com_ref::<IComponentHandler>() {
            unsafe { controller.setComponentHandler(h.as_ptr()) };
        }

        let ctl = Vst3Controller {
            controller: controller.clone(),
            param_count,
            has_editor: info.has_editor,
            view: None,
            listener,
            frame: ComWrapper::new(PlugFrame),
        };

        Ok(Self {
            info,
            layouts: vec![BusLayout::stereo()],
            active_layout: None,
            _module: module,
            component,
            processor,
            controller,
            separate_controller,
            component_cp,
            controller_cp,
            param_count,
            processing: false,
            ctl: Some(ctl),
            _host: host,
            _handler: handler,
            in_events: ComWrapper::new(EventList3::with_capacity(MAX_EVENTS)),
            out_events: ComWrapper::new(EventList3::with_capacity(MAX_EVENTS)),
            in_params: ComWrapper::new(ParamChanges::new()),
        })
    }
}

/// Call `IEditController::createView("editor")` and wrap the raw
/// pointer. Returns `None` when the plugin has no editor.
unsafe fn create_editor_view(controller: &ComPtr<IEditController>) -> Option<ComPtr<IPlugView>> {
    let raw = unsafe { controller.createView(ViewType::kEditor) };
    if raw.is_null() {
        return None;
    }
    unsafe { ComPtr::<IPlugView>::from_raw(raw) }
}

fn platform_type_for_handle(
    handle: truce_rack_core::editor::WindowHandle,
) -> (*const i8, *mut std::ffi::c_void) {
    use truce_rack_core::editor::WindowHandle;
    match handle {
        WindowHandle::NSView(p) => (kPlatformTypeNSView, p),
        WindowHandle::HWND(p) => (kPlatformTypeHWND, p),
        WindowHandle::X11(id) => (kPlatformTypeX11EmbedWindowID, id as *mut std::ffi::c_void),
    }
}

/// Run `factory.createInstance` for `I` and wrap the result as a
/// `ComPtr<I>`. The factory call uses the interface's `IID`
/// directly so the plugin returns the correct `*mut c_void`.
unsafe fn create_instance<I>(factory: &ComPtr<IPluginFactory>, cid: &TUID) -> Option<ComPtr<I>>
where
    I: vst3::Interface,
{
    let mut obj: *mut std::ffi::c_void = ptr::null_mut();
    // `com_scrape_types::Guid` and `TUID` share the same
    // `[i8; 16]` layout — reinterpret to avoid an extra copy.
    let iid_bytes: &TUID =
        unsafe { &*(std::ptr::from_ref::<vst3::com_scrape_types::Guid>(&I::IID).cast::<TUID>()) };
    let cid_ptr = cid.as_ptr();
    let iid_ptr = iid_bytes.as_ptr();
    if unsafe { factory.createInstance(cid_ptr, iid_ptr, &raw mut obj) } != kResultOk
        || obj.is_null()
    {
        return None;
    }
    unsafe { ComPtr::<I>::from_raw(obj.cast()) }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        if let Some(ctl) = self.ctl.as_mut() {
            truce_rack_core::editor::PluginEditor::close(ctl);
        }
        if self.processing {
            unsafe { self.processor.setProcessing(0) };
        }
        if self.active_layout.is_some() {
            unsafe { self.component.setActive(0) };
        }
        if let (Some(a), Some(b)) = (&self.component_cp, &self.controller_cp) {
            unsafe {
                a.disconnect(b.as_com_ref().as_ptr().cast());
                b.disconnect(a.as_com_ref().as_ptr().cast());
            }
        }
        if self.separate_controller {
            unsafe { self.controller.terminate() };
        }
        unsafe { self.component.terminate() };
    }
}

impl PluginCore for Vst3Plugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn active_layout(&self) -> Option<&BusLayout> {
        self.active_layout.as_ref()
    }
    fn supported_layouts(&self) -> &[BusLayout] {
        &self.layouts
    }

    fn parameter_count(&self) -> usize {
        self.param_count
    }

    fn parameter_info(&self, index: usize) -> Result<ParameterInfo> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        Ok(vst3_param_info_to_rack(&info))
    }

    fn parameter_value(&self, index: usize) -> Result<f64> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        Ok(unsafe { self.controller.getParamNormalized(info.id) })
    }

    fn parameter_value_string(&self, index: usize, _value: f64) -> Result<String> {
        // VST3 exposes IEditController::getParamStringByValue —
        // wiring it requires a TChar (UTF-16) buffer round-trip.
        // Tracked as a follow-on to avoid pulling in a wide-char
        // dep here.
        let _ = index;
        Err(Error::Other("vst3 parameter_value_string TODO".into()))
    }

    fn set_parameter(&mut self, index: usize, value: f64) -> Result<()> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        let clamped = value.clamp(0.0, 1.0);
        if unsafe { self.controller.setParamNormalized(info.id, clamped) } != kResultOk {
            return Err(Error::Other(
                "IEditController::setParamNormalized failed".into(),
            ));
        }
        Ok(())
    }

    fn preset_count(&self) -> usize {
        // VST3 exposes presets via the `IUnitInfo` interface,
        // wired in a follow-on. Treat as zero for now.
        0
    }
    fn preset_info(&self, index: usize) -> Result<PresetInfo> {
        Err(Error::InvalidParameter(index))
    }
    fn load_preset(&mut self, _preset_number: i32) -> Result<()> {
        Err(Error::Other("vst3 preset loading not yet wired".into()))
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        let stream = ComWrapper::new(MemoryStream::default());
        let stream_ptr = stream
            .to_com_ptr::<IBStream>()
            .ok_or_else(|| Error::Other("MemoryStream missing IBStream IID".into()))?;
        let status = unsafe { self.component.getState(stream_ptr.as_ptr()) };
        if status != kResultOk {
            return Err(Error::Other(format!(
                "IComponent::getState returned {status}"
            )));
        }
        Ok(stream.data.borrow().clone())
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<()> {
        let stream = ComWrapper::new(MemoryStream {
            data: std::cell::RefCell::new(bytes.to_vec()),
            position: std::cell::Cell::new(0),
        });
        let stream_ptr = stream
            .to_com_ptr::<IBStream>()
            .ok_or_else(|| Error::Other("MemoryStream missing IBStream IID".into()))?;
        let status = unsafe { self.component.setState(stream_ptr.as_ptr()) };
        if status != kResultOk {
            return Err(Error::Other(format!(
                "IComponent::setState returned {status}"
            )));
        }
        Ok(())
    }

    fn activate(
        &mut self,
        layout: BusLayout,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<()> {
        let mut input_arr = STEREO_ARRANGEMENT;
        let mut output_arr = STEREO_ARRANGEMENT;
        let _ = unsafe {
            self.processor
                .setBusArrangements(&raw mut input_arr, 1, &raw mut output_arr, 1)
        };

        let mut setup = ProcessSetup {
            #[allow(clippy::cast_possible_wrap)]
            processMode: ProcessModes_::kRealtime as i32,
            #[allow(clippy::cast_possible_wrap)]
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: i32::try_from(max_block_size).unwrap_or(i32::MAX),
            sampleRate: sample_rate,
        };
        if unsafe { self.processor.setupProcessing(&raw mut setup) } != kResultOk {
            return Err(Error::Other(
                "IAudioProcessor::setupProcessing failed".into(),
            ));
        }
        if unsafe { self.component.setActive(1) } != kResultOk {
            return Err(Error::Other("IComponent::setActive(true) failed".into()));
        }
        if unsafe { self.processor.setProcessing(1) } != kResultOk {
            return Err(Error::Other(
                "IAudioProcessor::setProcessing(true) failed".into(),
            ));
        }
        self.processing = true;
        self.active_layout = Some(layout);
        Ok(())
    }

    fn deactivate(&mut self) {
        if self.processing {
            unsafe { self.processor.setProcessing(0) };
            self.processing = false;
        }
        if self.active_layout.is_some() {
            unsafe { self.component.setActive(0) };
        }
        self.active_layout = None;
    }
    fn is_active(&self) -> bool {
        self.active_layout.is_some()
    }

    fn editor(&mut self) -> Option<&mut dyn truce_rack_core::editor::PluginEditor> {
        self.ctl.as_mut()?.editor()
    }

    fn take_controller(&mut self) -> Option<Box<dyn PluginController>> {
        self.ctl
            .take()
            .map(|c| Box::new(c) as Box<dyn PluginController>)
    }
}

impl PluginController for Vst3Controller {
    fn editor(&mut self) -> Option<&mut dyn truce_rack_core::editor::PluginEditor> {
        if self.has_editor { Some(self) } else { None }
    }

    fn parameter_count(&self) -> usize {
        self.param_count
    }

    fn parameter_info(&self, index: usize) -> Result<ParameterInfo> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        Ok(vst3_param_info_to_rack(&info))
    }

    fn parameter_value(&self, param_id: u32) -> Result<f64> {
        Ok(unsafe { self.controller.getParamNormalized(param_id) })
    }

    fn set_parameter_value(&mut self, param_id: u32, value: f64) -> Result<()> {
        if unsafe {
            self.controller
                .setParamNormalized(param_id, value.clamp(0.0, 1.0))
        } != kResultOk
        {
            return Err(Error::Other(
                "IEditController::setParamNormalized failed".into(),
            ));
        }
        Ok(())
    }

    fn set_change_listener(&mut self, listener: ChangeListener) {
        *self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(listener);
    }
}

impl truce_rack_core::editor::PluginEditor for Vst3Controller {
    fn open(
        &mut self,
        parent: truce_rack_core::editor::WindowHandle,
        _scale: f64,
    ) -> truce_rack_core::error::Result<()> {
        if self.view.is_some() {
            return Ok(());
        }
        let view = unsafe { create_editor_view(&self.controller) }
            .ok_or_else(|| Error::Other("IEditController::createView returned NULL".into()))?;
        let (type_str, parent_ptr) = platform_type_for_handle(parent);
        if unsafe { view.isPlatformTypeSupported(type_str) } != kResultOk {
            return Err(Error::Other(
                "IPlugView::isPlatformTypeSupported returned false".into(),
            ));
        }
        if let Some(frame) = self.frame.as_com_ref::<IPlugFrame>() {
            unsafe { view.setFrame(frame.as_ptr()) };
        }
        if unsafe { view.attached(parent_ptr, type_str) } != kResultOk {
            return Err(Error::Other("IPlugView::attached returned non-OK".into()));
        }
        self.view = Some(view);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(view) = self.view.take() {
            unsafe {
                view.removed();
                view.setFrame(ptr::null_mut());
            }
        }
    }

    fn is_open(&self) -> bool {
        self.view.is_some()
    }

    fn size(&self) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { view.getSize(&raw mut rect) } != kResultOk {
            return None;
        }
        let w = u32::try_from(rect.right - rect.left).ok()?;
        let h = u32::try_from(rect.bottom - rect.top).ok()?;
        Some((w, h))
    }

    fn is_resizable(&self) -> bool {
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        unsafe { view.canResize() == kResultOk }
    }

    fn set_size(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: i32::try_from(width).ok()?,
            bottom: i32::try_from(height).ok()?,
        };
        // Plugins may snap to a constraint via checkSizeConstraint.
        let _ = unsafe { view.checkSizeConstraint(&raw mut rect) };
        if unsafe { view.onSize(&raw mut rect) } != kResultOk {
            return None;
        }
        let w = u32::try_from(rect.right - rect.left).ok()?;
        let h = u32::try_from(rect.bottom - rect.top).ok()?;
        Some((w, h))
    }

    fn show(&mut self) {
        // VST3 attaches once and stays visible — no show/hide
        // separate from attached/removed.
    }

    fn hide(&mut self) {
        // Same as show — VST3 has no distinct hide.
    }
}

fn empty_parameter_info() -> Vst3ParameterInfo {
    Vst3ParameterInfo {
        id: 0,
        title: [0; 128],
        shortTitle: [0; 128],
        units: [0; 128],
        stepCount: 0,
        defaultNormalizedValue: 0.0,
        unitId: 0,
        flags: 0,
    }
}

fn vst3_param_info_to_rack(info: &Vst3ParameterInfo) -> ParameterInfo {
    let name = string128_to_string(&info.title);
    let short_name = string128_to_string(&info.shortTitle);
    let unit = string128_to_string(&info.units);
    let mut flags = truce_rack_core::info::ParameterFlags::empty();
    if info.flags & ParameterFlags_::kIsBypass != 0 {
        flags |= truce_rack_core::info::ParameterFlags::BYPASS;
    }
    if info.flags & ParameterFlags_::kCanAutomate != 0 {
        flags |= truce_rack_core::info::ParameterFlags::AUTOMATABLE;
    }
    if info.flags & ParameterFlags_::kIsHidden != 0 {
        flags |= truce_rack_core::info::ParameterFlags::HIDDEN;
    }
    if info.flags & ParameterFlags_::kIsReadOnly != 0 {
        flags |= truce_rack_core::info::ParameterFlags::READ_ONLY;
    }
    if info.flags & ParameterFlags_::kIsList != 0 {
        flags |= truce_rack_core::info::ParameterFlags::ENUMERATED;
    }
    ParameterInfo {
        id: info.id,
        name,
        short_name,
        unit,
        min: 0.0,
        max: 1.0,
        default: info.defaultNormalizedValue,
        step_count: u32::try_from(info.stepCount).unwrap_or(0),
        flags,
    }
}

/// VST3 `String128` is `[char16; 128]` (UTF-16). Walk the slice
/// until the first NUL and decode to UTF-8.
fn string128_to_string(buf: &[u16; 128]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Translate a host [`TransportInfo`] snapshot into Steinberg's
/// `Vst::ProcessContext`. Only the fields the host reported get
/// their `k…Valid` flag set; everything else stays zero so the
/// plugin treats it as absent.
// `StatesAndFlags` is `c_int` or `c_uint` depending on platform, so
// the `as u32` casts are only redundant on some targets.
#[allow(clippy::unnecessary_cast)]
fn build_vst3_context(t: &TransportInfo, sample_rate: f64) -> Vst3ProcessContext {
    // SAFETY: ProcessContext is a repr(C) aggregate of integers,
    // floats, and small POD sub-structs (Chord, FrameRate). An
    // all-zero value is the valid "no flags set" state; we then
    // fill in the fields the host actually reported.
    let mut ctx: Vst3ProcessContext = unsafe { std::mem::zeroed() };
    ctx.sampleRate = sample_rate;

    let mut state: u32 = 0;
    if let Some(tempo) = t.tempo_bpm {
        ctx.tempo = tempo;
        state |= StatesAndFlags_::kTempoValid as u32;
    }
    if let Some((num, den)) = t.time_signature {
        ctx.timeSigNumerator = i32::try_from(num).unwrap_or(4);
        ctx.timeSigDenominator = i32::try_from(den).unwrap_or(4);
        state |= StatesAndFlags_::kTimeSigValid as u32;
    }
    if let Some(beats) = t.song_position_beats {
        // projectTimeMusic is in quarter notes (TQuarterNotes = f64).
        ctx.projectTimeMusic = beats;
        state |= StatesAndFlags_::kProjectTimeMusicValid as u32;
    }
    if let Some(samples) = t.song_position_samples {
        ctx.projectTimeSamples = samples;
    }
    if let Some(bar) = t.bar_start_beats {
        ctx.barPositionMusic = bar;
        state |= StatesAndFlags_::kBarPositionValid as u32;
    }
    if t.playing {
        state |= StatesAndFlags_::kPlaying as u32;
    }
    if t.recording {
        state |= StatesAndFlags_::kRecording as u32;
    }
    if t.loop_active {
        state |= StatesAndFlags_::kCycleActive as u32;
    }
    ctx.state = state;
    ctx
}

impl Plugin<f32> for Vst3Plugin {
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<'_, f32>,
        events: &EventList,
        context: &mut ProcessContext<'_>,
    ) -> Result<ProcessStatus> {
        if !self.is_active() {
            return Err(Error::NotActivated);
        }
        let frames = buffer.num_frames();

        // Translate events into the pre-allocated lists: MIDI into the
        // IEventList, parameter values into IParameterChanges.
        {
            let mut midi = self.in_events.events.borrow_mut();
            midi.clear();
            self.in_params.clear();
            for event in events {
                if let truce_rack_core::events::EventBody::ParamValue { param_id, value } =
                    event.body
                {
                    let offset = i32::try_from(event.sample_offset).unwrap_or(0);
                    self.in_params.add(param_id, offset, value);
                } else if midi.len() < midi.capacity()
                    && let Some(ev) = translate_event(event)
                {
                    midi.push(ev);
                }
            }
            self.out_events.events.borrow_mut().clear();
        }
        let in_events = self
            .in_events
            .as_com_ref::<IEventList>()
            .map_or(ptr::null_mut(), |e| e.as_ptr());
        let out_events = self
            .out_events
            .as_com_ref::<IEventList>()
            .map_or(ptr::null_mut(), |e| e.as_ptr());
        let in_params = self
            .in_params
            .as_com_ref::<IParameterChanges>()
            .map_or(ptr::null_mut(), |e| e.as_ptr());

        let mut input_ptrs = [ptr::null_mut::<f32>(); MAX_CHANNELS];
        let main_inputs = buffer.main_inputs();
        let n_in = main_inputs.len().min(MAX_CHANNELS);
        for (p, c) in input_ptrs.iter_mut().zip(main_inputs.iter()) {
            *p = c.as_ptr().cast_mut();
        }
        let mut input_bus = AudioBusBuffers {
            numChannels: i32::try_from(n_in).unwrap_or(0),
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: input_ptrs.as_mut_ptr(),
            },
        };

        let mut output_ptrs = [ptr::null_mut::<f32>(); MAX_CHANNELS];
        let main_outputs = buffer.main_outputs();
        let n_out = main_outputs.len().min(MAX_CHANNELS);
        for (p, c) in output_ptrs.iter_mut().zip(main_outputs.iter_mut()) {
            *p = c.as_mut_ptr();
        }
        let mut output_bus = AudioBusBuffers {
            numChannels: i32::try_from(n_out).unwrap_or(0),
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: output_ptrs.as_mut_ptr(),
            },
        };

        // Build the transport context up front so its backing
        // struct outlives the process call.
        let mut process_context = context
            .transport
            .map(|t| build_vst3_context(&t, context.sample_rate));

        let mut data = ProcessData {
            #[allow(clippy::cast_possible_wrap)]
            processMode: ProcessModes_::kRealtime as i32,
            #[allow(clippy::cast_possible_wrap)]
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: i32::try_from(frames).unwrap_or(i32::MAX),
            numInputs: 1,
            numOutputs: 1,
            inputs: &raw mut input_bus,
            outputs: &raw mut output_bus,
            inputParameterChanges: in_params,
            outputParameterChanges: ptr::null_mut::<IParameterChanges>(),
            inputEvents: in_events,
            outputEvents: out_events,
            processContext: process_context
                .as_mut()
                .map_or(ptr::null_mut(), std::ptr::from_mut),
        };

        let processor_ptr = self.processor.as_ptr();
        let status = run_audio_block_with::<Vst3Plugin, i32>(FORMAT, -1, || unsafe {
            ((*(*processor_ptr).vtbl).process)(processor_ptr, &raw mut data)
        });
        if status == kResultOk {
            Ok(ProcessStatus::Continue)
        } else {
            Ok(ProcessStatus::Error)
        }
    }
}

// ---------------------------------------------------------------------------
// EventList3 — in-memory IEventList impl for MIDI in/out.
// ---------------------------------------------------------------------------

/// Translate one truce-rack MIDI event into a VST3 `Event`. Returns `None`
/// for events VST3 doesn't carry in an `IEventList`.
fn translate_event(event: &truce_rack_core::events::Event) -> Option<Event> {
    use truce_rack_core::events::{EventBody, MidiData};
    let offset = i32::try_from(event.sample_offset).unwrap_or(i32::MAX);
    let EventBody::Midi(body) = event.body else {
        return None;
    };
    let header = |type_: u16| Event {
        busIndex: 0,
        sampleOffset: offset,
        ppqPosition: 0.0,
        flags: 0,
        r#type: type_,
        __field0: Event__type0 {
            noteOn: NoteOnEvent {
                channel: 0,
                pitch: 0,
                tuning: 0.0,
                velocity: 0.0,
                length: 0,
                noteId: -1,
            },
        },
    };
    match body {
        MidiData::NoteOn {
            channel,
            note,
            velocity,
        } => {
            let mut ev = header(u16::try_from(EventTypes_::kNoteOnEvent).unwrap_or(0));
            ev.__field0 = Event__type0 {
                noteOn: NoteOnEvent {
                    channel: i16::from(channel),
                    pitch: i16::from(note),
                    tuning: 0.0,
                    velocity: f32::from(velocity) / 127.0,
                    length: 0,
                    noteId: -1,
                },
            };
            return Some(ev);
        }
        MidiData::NoteOff {
            channel,
            note,
            velocity,
        } => {
            let mut ev = header(u16::try_from(EventTypes_::kNoteOffEvent).unwrap_or(0));
            ev.__field0 = Event__type0 {
                noteOff: NoteOffEvent {
                    channel: i16::from(channel),
                    pitch: i16::from(note),
                    velocity: f32::from(velocity) / 127.0,
                    noteId: -1,
                    tuning: 0.0,
                },
            };
            return Some(ev);
        }
        MidiData::PolyAftertouch {
            channel,
            note,
            pressure,
        } => {
            let mut ev = header(u16::try_from(EventTypes_::kPolyPressureEvent).unwrap_or(0));
            ev.__field0 = Event__type0 {
                polyPressure: PolyPressureEvent {
                    channel: i16::from(channel),
                    pitch: i16::from(note),
                    pressure: f32::from(pressure) / 127.0,
                    noteId: -1,
                },
            };
            return Some(ev);
        }
        // VST3 routes CC / ProgramChange / ChannelAftertouch /
        // PitchBend / Sysex through IParameterChanges or
        // IMidiMapping rather than IEventList. Wiring those is
        // a follow-on; for hosting test purposes notes are the
        // primary need.
        _ => {}
    }
    None
}

#[derive(Default)]
struct EventList3 {
    events: RefCell<Vec<Event>>,
}

impl EventList3 {
    fn with_capacity(n: usize) -> Self {
        Self {
            events: RefCell::new(Vec::with_capacity(n)),
        }
    }
}

impl Class for EventList3 {
    type Interfaces = (IEventList,);
}

#[allow(clippy::cast_sign_loss)]
impl IEventListTrait for EventList3 {
    unsafe fn getEventCount(&self) -> i32 {
        i32::try_from(self.events.borrow().len()).unwrap_or(i32::MAX)
    }

    unsafe fn getEvent(&self, index: i32, out: *mut Event) -> i32 {
        if out.is_null() || index < 0 {
            return -1;
        }
        let events = self.events.borrow();
        let Some(event) = events.get(index as usize) else {
            return -1;
        };
        unsafe { *out = *event };
        kResultOk
    }

    unsafe fn addEvent(&self, event: *mut Event) -> i32 {
        let mut events = self.events.borrow_mut();
        // Never grow on the audio thread.
        if event.is_null() || events.len() == events.capacity() {
            return kResultFalse;
        }
        events.push(unsafe { *event });
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// ParamChanges — pre-allocated IParameterChanges for host → processor values.
// ---------------------------------------------------------------------------

const MAX_CHANNELS: usize = 8;
const MAX_EVENTS: usize = 512;
const MAX_PARAM_QUEUES: usize = 64;
const MAX_POINTS_PER_QUEUE: usize = 16;

struct ParamQueue {
    id: Cell<ParamID>,
    points: RefCell<Vec<(i32, ParamValue)>>,
}

// SAFETY (EventList3, ParamQueue, ParamChanges): these are only touched by
// `Vst3Plugin::process` and by the plugin during that call, which holds
// `&mut Vst3Plugin` — never from two threads at once.
unsafe impl Sync for EventList3 {}
unsafe impl Sync for ParamQueue {}
unsafe impl Sync for ParamChanges {}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

#[allow(clippy::cast_sign_loss)]
impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> i32 {
        i32::try_from(self.points.borrow().len()).unwrap_or(0)
    }

    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut ParamValue) -> i32 {
        let points = self.points.borrow();
        let Some(&(o, v)) = usize::try_from(index).ok().and_then(|i| points.get(i)) else {
            return kResultFalse;
        };
        if offset.is_null() || value.is_null() {
            return kResultFalse;
        }
        unsafe {
            *offset = o;
            *value = v;
        }
        kResultOk
    }

    unsafe fn addPoint(&self, offset: i32, value: ParamValue, index: *mut i32) -> i32 {
        let mut points = self.points.borrow_mut();
        if points.len() == points.capacity() {
            return kResultFalse;
        }
        points.push((offset, value));
        if !index.is_null() {
            unsafe { *index = i32::try_from(points.len() - 1).unwrap_or(0) };
        }
        kResultOk
    }
}

struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    used: Cell<usize>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    fn new() -> Self {
        let queues = (0..MAX_PARAM_QUEUES)
            .map(|_| {
                ComWrapper::new(ParamQueue {
                    id: Cell::new(0),
                    points: RefCell::new(Vec::with_capacity(MAX_POINTS_PER_QUEUE)),
                })
            })
            .collect();
        Self {
            queues,
            used: Cell::new(0),
        }
    }

    fn clear(&self) {
        for q in &self.queues[..self.used.get()] {
            q.points.borrow_mut().clear();
        }
        self.used.set(0);
    }

    /// Index of the queue for `id`, claiming a free one if needed.
    fn queue_for(&self, id: ParamID) -> Option<usize> {
        let used = self.used.get();
        if let Some(i) = self.queues[..used].iter().position(|q| q.id.get() == id) {
            return Some(i);
        }
        if used == self.queues.len() {
            return None;
        }
        self.queues[used].id.set(id);
        self.used.set(used + 1);
        Some(used)
    }

    /// Queue `value` for `id` at `offset`. Drops the point when full.
    fn add(&self, id: ParamID, offset: i32, value: ParamValue) {
        if let Some(i) = self.queue_for(id) {
            unsafe { self.queues[i].addPoint(offset, value, ptr::null_mut()) };
        }
    }

    fn queue_ptr(&self, i: usize) -> *mut IParamValueQueue {
        self.queues[i]
            .as_com_ref::<IParamValueQueue>()
            .map_or(ptr::null_mut(), |q| q.as_ptr())
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> i32 {
        i32::try_from(self.used.get()).unwrap_or(0)
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        match usize::try_from(index) {
            Ok(i) if i < self.used.get() => self.queue_ptr(i),
            _ => ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut i32,
    ) -> *mut IParamValueQueue {
        if id.is_null() {
            return ptr::null_mut();
        }
        let Some(i) = self.queue_for(unsafe { *id }) else {
            return ptr::null_mut();
        };
        if !index.is_null() {
            unsafe { *index = i32::try_from(i).unwrap_or(0) };
        }
        self.queue_ptr(i)
    }
}

// ---------------------------------------------------------------------------
// Host-side COM objects: application context, component handler, plug frame.
// ---------------------------------------------------------------------------

type SharedListener = Arc<Mutex<Option<ChangeListener>>>;

/// UI / parameter side of a [`Vst3Plugin`], see [`PluginController`].
pub struct Vst3Controller {
    controller: ComPtr<IEditController>,
    param_count: usize,
    has_editor: bool,
    view: Option<ComPtr<IPlugView>>,
    listener: SharedListener,
    frame: ComWrapper<PlugFrame>,
}

// SAFETY: the controller is only used from one thread at a time (the host's
// UI thread); VST3 objects aren't tied to the thread that created them.
unsafe impl Send for Vst3Controller {}

struct HostApplication;

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> i32 {
        if name.is_null() {
            return kResultFalse;
        }
        let out = unsafe { &mut *name };
        out.fill(0);
        for (o, c) in out.iter_mut().zip("PluginHost".encode_utf16()) {
            *o = c;
        }
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut std::ffi::c_void,
    ) -> i32 {
        if !obj.is_null() {
            unsafe { *obj = ptr::null_mut() };
        }
        kResultFalse
    }
}

struct ComponentHandler {
    listener: SharedListener,
}

impl ComponentHandler {
    fn emit(&self, param_id: ParamID, value: f64, kind: ChangeKind) -> i32 {
        if let Some(f) = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            f(ParamChange {
                param_id,
                value,
                kind,
            });
        }
        kResultOk
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> i32 {
        self.emit(id, 0.0, ChangeKind::Begin)
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> i32 {
        self.emit(id, value, ChangeKind::Value)
    }

    unsafe fn endEdit(&self, id: ParamID) -> i32 {
        self.emit(id, 0.0, ChangeKind::End)
    }

    unsafe fn restartComponent(&self, _flags: i32) -> i32 {
        kResultOk
    }
}

struct PlugFrame;

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> i32 {
        // Accept the plugin's requested size; hosts read it back via `size()`.
        match unsafe { ComRef::from_raw(view) } {
            Some(v) if !new_size.is_null() => unsafe { v.onSize(new_size) },
            _ => kResultFalse,
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryStream — in-memory IBStream impl for state save/load.
// ---------------------------------------------------------------------------

/// Backing storage for the `IBStream` we hand to
/// `IComponent::setState` / `getState`. The plugin reads and
/// writes through `read`/`write`; the host inspects
/// `data` / `position` after the call.
#[derive(Default)]
struct MemoryStream {
    data: std::cell::RefCell<Vec<u8>>,
    position: std::cell::Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_read: *mut i32,
    ) -> i32 {
        if buffer.is_null() || num_bytes < 0 {
            return -1;
        }
        let pos = self.position.get();
        #[allow(clippy::cast_sign_loss)]
        let want = num_bytes as usize;
        let data = self.data.borrow();
        let available = data.len().saturating_sub(pos);
        let take = want.min(available);
        if take > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer.cast::<u8>(), take);
            }
        }
        self.position.set(pos + take);
        if !num_bytes_read.is_null() {
            unsafe {
                *num_bytes_read = i32::try_from(take).unwrap_or(i32::MAX);
            }
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> i32 {
        if buffer.is_null() || num_bytes < 0 {
            return -1;
        }
        let pos = self.position.get();
        #[allow(clippy::cast_sign_loss)]
        let want = num_bytes as usize;
        let mut data = self.data.borrow_mut();
        if data.len() < pos + want {
            data.resize(pos + want, 0);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(buffer.cast::<u8>(), data.as_mut_ptr().add(pos), want);
        }
        self.position.set(pos + want);
        if !num_bytes_written.is_null() {
            unsafe {
                *num_bytes_written = i32::try_from(want).unwrap_or(i32::MAX);
            }
        }
        kResultOk
    }

    #[allow(clippy::cast_possible_wrap)]
    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> i32 {
        // VST3 SDK SeekMode: 0 = `SeekSet`, 1 = `SeekCur`, 2 = `SeekEnd`.
        let data_len = self.data.borrow().len() as i64;
        let current = self.position.get() as i64;
        let new_pos = match mode {
            0 => pos,
            1 => current + pos,
            2 => data_len + pos,
            _ => return -1,
        };
        if new_pos < 0 {
            return -1;
        }
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        self.position.set(new_pos as usize);
        if !result.is_null() {
            unsafe {
                *result = new_pos;
            }
        }
        kResultOk
    }

    #[allow(clippy::cast_possible_wrap)]
    unsafe fn tell(&self, pos: *mut i64) -> i32 {
        if pos.is_null() {
            return -1;
        }
        unsafe {
            *pos = self.position.get() as i64;
        }
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn category_from_vst3_subcategories() {
        assert_eq!(
            category_from_subcategories("Instrument|Synth"),
            PluginCategory::Instrument
        );
        assert_eq!(
            category_from_subcategories("Fx|Reverb"),
            PluginCategory::Effect
        );
        assert_eq!(
            category_from_subcategories("Fx|Analyzer"),
            PluginCategory::Analyzer
        );
        assert_eq!(category_from_subcategories(""), PluginCategory::Effect);
    }

    #[test]
    fn component_handler_forwards_edits_to_listener() {
        let listener: SharedListener = Arc::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        *listener.lock().unwrap() = Some(Box::new(move |c| sink.lock().unwrap().push(c)));
        let handler = ComWrapper::new(ComponentHandler { listener });
        let h = handler.to_com_ptr::<IComponentHandler>().unwrap();
        unsafe {
            h.beginEdit(7);
            h.performEdit(7, 0.25);
            h.endEdit(7);
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[1],
            ParamChange {
                param_id: 7,
                value: 0.25,
                kind: ChangeKind::Value
            }
        );
        assert_eq!(
            (seen[0].kind, seen[2].kind),
            (ChangeKind::Begin, ChangeKind::End)
        );
    }

    #[test]
    fn param_changes_group_points_by_param_and_clear() {
        let changes = ComWrapper::new(ParamChanges::new());
        changes.add(3, 0, 0.1);
        changes.add(9, 5, 0.9);
        changes.add(3, 64, 0.2);
        let c = changes.to_com_ptr::<IParameterChanges>().unwrap();
        unsafe {
            assert_eq!(c.getParameterCount(), 2);
            let q = ComRef::from_raw(c.getParameterData(0)).unwrap();
            assert_eq!((q.getParameterId(), q.getPointCount()), (3, 2));
            let (mut off, mut val) = (0, 0.0);
            q.getPoint(1, &raw mut off, &raw mut val);
            assert_eq!((off, val), (64, 0.2));
            assert!(c.getParameterData(2).is_null());
        }
        changes.clear();
        assert_eq!(unsafe { c.getParameterCount() }, 0);
    }

    #[test]
    fn param_changes_drop_points_when_full_instead_of_growing() {
        let changes = ComWrapper::new(ParamChanges::new());
        for i in 0..21 {
            changes.add(1, i, 0.5);
        }
        assert_eq!(
            changes.queues[0].points.borrow().len(),
            MAX_POINTS_PER_QUEUE
        );
        assert_eq!(
            changes.queues[0].points.borrow().capacity(),
            MAX_POINTS_PER_QUEUE
        );
    }

    use super::*;

    #[test]
    fn tuid_hex_roundtrip() {
        let cid: TUID = [
            0x01, 0x23, 0x45, 0x67, -0x10, -0x10, -0x10, -0x10, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let hex = tuid_to_hex(&cid);
        let parsed = hex_to_tuid(&hex).expect("parse");
        assert_eq!(parsed, cid);
    }

    #[test]
    fn char8_skips_trailing_nul() {
        let mut arr = [0i8; 16];
        for (i, &b) in b"Hello".iter().enumerate() {
            // ASCII byte → i8 always fits losslessly; the cast is
            // bit-pattern-identical for `b` < 128.
            arr[i] = i8::try_from(b).expect("ASCII byte fits in i8");
        }
        assert_eq!(char8_array_to_string(&arr), "Hello");
    }
}
