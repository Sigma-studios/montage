//! The outputs' names and models from Wayland (`wl_output`; the `name` and
//! `description` events are version 4, the make and model of `geometry`
//! version 1), so the screen picker can say "DP-5 — Dell U2720Q".

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use super::probe::{Output, output_model};

#[derive(Default)]
struct Found {
    name: String,
    description: String,
    make: String,
    model: String,
}

#[derive(Default)]
struct State {
    outputs: Vec<Found>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut State,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut State,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let Some(found) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { make, model, .. } => {
                found.make = make;
                found.model = model;
            }
            wl_output::Event::Name { name } => found.name = name,
            wl_output::Event::Description { description } => found.description = description,
            _ => {}
        }
    }
}

/// The outputs of the running Wayland session; an error outside one.
pub fn outputs() -> Result<Vec<Output>, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = State::default();
    let mut bound = Vec::new();
    for global in globals.contents().clone_list() {
        if global.interface != wl_output::WlOutput::interface().name {
            continue;
        }
        let index = state.outputs.len();
        state.outputs.push(Found::default());
        let output: wl_output::WlOutput =
            globals
                .registry()
                .bind(global.name, global.version.min(4), &qh, index);
        bound.push(output);
    }
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    for output in bound {
        if output.version() >= 3 {
            output.release();
        }
    }
    let _ = conn.flush();
    Ok(state
        .outputs
        .into_iter()
        .filter(|o| !o.name.is_empty())
        .map(|o| Output {
            model: output_model(&o.name, &o.description, &o.make, &o.model),
            name: o.name,
        })
        .collect())
}
