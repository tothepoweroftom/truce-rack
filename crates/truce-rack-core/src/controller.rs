//! The UI / parameter side of a plugin, split from its audio side.
//!
//! Hosts that run `process` on a dedicated audio thread can't also borrow the
//! plugin for its editor or parameter reads. [`PluginCore::take_controller`]
//! hands out a [`PluginController`] that owns the editor and parameter queries
//! and lives on the UI (main) thread, while the plugin itself keeps `process`.
//!
//! [`PluginCore::take_controller`]: crate::plugin::PluginCore::take_controller

use crate::editor::PluginEditor;
use crate::error::Result;
use crate::info::ParameterInfo;

/// Which part of a user gesture a [`ParamChange`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The user started a gesture (e.g. mouse down on a knob).
    Begin,
    /// The value changed.
    Value,
    /// The gesture ended.
    End,
}

/// A parameter change made by the plugin itself, usually from its own UI.
/// `value` is in the parameter's native range (see [`ParameterInfo`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamChange {
    /// Parameter id from [`ParameterInfo::id`].
    pub param_id: u32,
    /// New value, native range.
    pub value: f64,
    /// Gesture phase.
    pub kind: ChangeKind,
}

/// Callback for plugin-originated parameter changes. May be called from the UI
/// thread or, for some formats, another plugin thread.
pub type ChangeListener = Box<dyn Fn(ParamChange) + Send + Sync>;

/// UI and parameter access for a loaded plugin. Use from the UI thread.
///
/// Drop the controller before the plugin it came from.
pub trait PluginController: Send {
    /// The plugin's editor, if it has one.
    fn editor(&mut self) -> Option<&mut dyn PluginEditor>;
    /// Number of parameters.
    fn parameter_count(&self) -> usize;
    /// Metadata for the parameter at `index` (`0..parameter_count()`).
    ///
    /// # Errors
    /// [`crate::Error::InvalidParameter`] when `index` is out of range.
    fn parameter_info(&self, index: usize) -> Result<ParameterInfo>;
    /// Current value of parameter `param_id`, native range.
    ///
    /// # Errors
    /// When the plugin doesn't know `param_id`.
    fn parameter_value(&self, param_id: u32) -> Result<f64>;
    /// Tell the UI side about a host-made change so the editor follows. This
    /// does not reach the audio side: send an `EventBody::ParamValue` to
    /// `process` for that.
    ///
    /// # Errors
    /// When the plugin rejects the value.
    fn set_parameter_value(&mut self, param_id: u32, value: f64) -> Result<()>;
    /// Replace the listener for plugin-originated changes.
    fn set_change_listener(&mut self, listener: ChangeListener);
}
