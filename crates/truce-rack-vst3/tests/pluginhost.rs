#![allow(clippy::cast_precision_loss)]
//! Real-plugin checks for the `PluginHost` fixes. Ignored by default; run with
//! `PLUGINHOST_TEST_VST3=/Library/Audio/Plug-Ins/VST3/Vital.vst3 cargo test -p truce-rack-vst3 -- --ignored`.

use std::path::Path;

use truce_rack_core::buffer::{AudioBuffer, BusRange};
use truce_rack_core::bus::BusLayout;
use truce_rack_core::events::{Event, EventBody, EventList, MidiData};
use truce_rack_core::plugin::{Plugin, PluginCore, ProcessContext};
use truce_rack_core::scanner::PluginScanner;
use truce_rack_vst3::{Vst3Plugin, Vst3Scanner};

const FRAMES: usize = 128;

fn load() -> Option<Vst3Plugin> {
    let path = std::env::var("PLUGINHOST_TEST_VST3").ok()?;
    let path = Path::new(&path);
    let scanner = Vst3Scanner::new();
    let infos = scanner.scan_path(path.parent().unwrap()).unwrap();
    let info = infos
        .into_iter()
        .find(|i| i.path.starts_with(path))
        .expect("plugin not in scan");
    let mut p = scanner.load(&info).unwrap();
    p.activate(BusLayout::stereo(), 48_000.0, FRAMES).unwrap();
    Some(p)
}

/// Renders `blocks` blocks and returns the RMS of the last one.
fn render(p: &mut Vst3Plugin, blocks: usize, first_events: &EventList) -> f32 {
    let input = vec![0.0f32; FRAMES];
    let (mut l, mut r) = (vec![0.0f32; FRAMES], vec![0.0f32; FRAMES]);
    let empty = EventList::new();
    for b in 0..blocks {
        let inputs: [&[f32]; 2] = [&input, &input];
        let mut outputs: [&mut [f32]; 2] = [&mut l, &mut r];
        let bus = [BusRange::new(0, 2)];
        let mut buffer = AudioBuffer::new(&inputs, &mut outputs, FRAMES, &bus, &bus);
        let mut out_events = EventList::new();
        let mut ctx = ProcessContext {
            sample_rate: 48_000.0,
            max_block_size: FRAMES,
            transport: None,
            output_events: &mut out_events,
        };
        p.process(
            &mut buffer,
            if b == 0 { first_events } else { &empty },
            &mut ctx,
        )
        .unwrap();
    }
    (l.iter().map(|s| s * s).sum::<f32>() / FRAMES as f32).sqrt()
}

fn param_id_named(p: &Vst3Plugin, name: &str) -> u32 {
    (0..p.parameter_count())
        .map(|i| p.parameter_info(i).unwrap())
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("no parameter {name:?}"))
        .id
}

#[test]
#[ignore = "needs PLUGINHOST_TEST_VST3"]
fn param_value_event_changes_rendered_output() {
    let Some(mut p) = load() else { return };
    let note_on = EventList::from_slice(&[Event {
        sample_offset: 0,
        body: EventBody::Midi(MidiData::NoteOn {
            channel: 0,
            note: 60,
            velocity: 110,
        }),
    }]);
    let loud = render(&mut p, 40, &note_on);
    assert!(loud > 1e-4, "synth silent: rms {loud}");

    let volume = param_id_named(&p, "Volume");
    let quiet_events = EventList::from_slice(&[Event {
        sample_offset: 0,
        body: EventBody::ParamValue {
            param_id: volume,
            value: 0.0,
        },
    }]);
    let quiet = render(&mut p, 40, &quiet_events);
    let db = 20.0 * (quiet.max(1e-9) / loud).log10();
    assert!(
        db < -1.0,
        "volume event had no effect: {db:.1} dB (rms {loud} -> {quiet})"
    );
}

#[test]
#[ignore = "needs PLUGINHOST_TEST_VST3"]
fn take_controller_returns_some_once() {
    let Some(mut p) = load() else { return };
    let ctl = p.take_controller();
    assert!(ctl.is_some());
    assert!(p.take_controller().is_none());
    assert!(p.editor().is_none(), "editor stays with the controller");
    let ctl = ctl.unwrap();
    assert_eq!(ctl.parameter_count(), p.parameter_count());
    drop(ctl);
}
