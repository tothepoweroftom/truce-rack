#![allow(clippy::cast_precision_loss)]
//! Real-plugin checks for the `PluginHost` fixes. Ignored by default; run with
//! `PLUGINHOST_TEST_AU="Ablaze Wavetable" cargo test -p truce-rack-au -- --ignored --test-threads=1`.

use truce_rack_au::{AuPlugin, AuScanner};
use truce_rack_core::buffer::{AudioBuffer, BusRange};
use truce_rack_core::bus::BusLayout;
use truce_rack_core::events::{Event, EventBody, EventList, MidiData};
use truce_rack_core::plugin::{Plugin, PluginCore, ProcessContext};
use truce_rack_core::scanner::PluginScanner;

const FRAMES: usize = 128;

fn load() -> Option<AuPlugin> {
    let name = std::env::var("PLUGINHOST_TEST_AU").ok()?;
    let scanner = AuScanner::new();
    let info = scanner
        .scan()
        .unwrap()
        .into_iter()
        .find(|i| i.name == name)
        .expect("AU not found");
    let mut p = scanner.load(&info).unwrap();
    p.activate(BusLayout::stereo(), 48_000.0, FRAMES).unwrap();
    Some(p)
}

fn render(p: &mut AuPlugin, blocks: usize, first_events: &EventList) -> f32 {
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

/// First parameter whose name looks like an output level control.
fn level_param(p: &AuPlugin) -> (u32, f64) {
    let infos: Vec<_> = (0..p.parameter_count())
        .map(|i| p.parameter_info(i).unwrap())
        .collect();
    let pick = infos
        .iter()
        .find(|i| {
            let n = i.name.to_lowercase();
            ["master", "volume", "gain", "level", "output"]
                .iter()
                .any(|w| n.contains(w))
        })
        .unwrap_or_else(|| {
            panic!(
                "no level-like parameter in {:?}",
                infos.iter().map(|i| &i.name).collect::<Vec<_>>()
            )
        });
    (pick.id, pick.min)
}

#[test]
#[ignore = "needs PLUGINHOST_TEST_AU"]
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
    let (id, min) = level_param(&p);
    let events = EventList::from_slice(&[Event {
        sample_offset: 0,
        body: EventBody::ParamValue {
            param_id: id,
            value: min,
        },
    }]);
    let quiet = render(&mut p, 40, &events);
    let db = 20.0 * (quiet.max(1e-9) / loud).log10();
    assert!(db < -1.0, "param event had no effect: {db:.1} dB");
}

#[test]
#[ignore = "needs PLUGINHOST_TEST_AU"]
fn take_controller_returns_some_once() {
    let Some(mut p) = load() else { return };
    let ctl = p.take_controller().expect("controller");
    assert!(p.take_controller().is_none());
    assert!(p.editor().is_none());
    assert_eq!(ctl.parameter_count(), p.parameter_count());
    let (id, _) = level_param(&p);
    assert!(ctl.parameter_value(id).is_ok());
}
