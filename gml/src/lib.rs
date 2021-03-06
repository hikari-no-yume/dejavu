#![feature(maybe_uninit_extra)]
#![feature(box_patterns)]
#![feature(box_syntax)]
#![feature(extern_types)]
#![feature(untagged_unions)]

use std::collections::HashMap;
use std::{fmt, io};

use crate::symbol::Symbol;
use crate::front::{Lexer, Parser, ActionParser, Lines, Position, Span};
use crate::back::ssa;
use crate::vm::code;

pub use gml_meta::bind;

#[macro_use]
mod handle_map;
mod rc_vec;
mod bit_vec;
pub mod symbol;

pub mod front;
pub mod back;
pub mod vm;

/// The name of a single executable unit of GML or D&D actions.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Function {
    Event { object_index: i32, event_type: u32, event_kind: i32 },
    Script { id: i32 },
}

/// An entity defined by the engine.
pub enum Item<W, A> {
    Native(vm::ApiFunction<W, A>, usize, bool),
    Member(Option<vm::GetFunction<W, A>>, Option<vm::SetFunction<W, A>>),
}

/// Build the GML and D&D in a Game Maker project.
pub fn build<W, A, F: FnMut() -> E, E: io::Write + 'static>(
    game: &project::Game, engine: &HashMap<Symbol, Item<W, A>>, mut errors: F
) -> Result<(vm::Assets<W, A>, vm::Debug), u32> {
    let mut assets = vm::Assets::default();
    let mut prototypes = HashMap::with_capacity(game.scripts.len() + engine.len());
    let mut debug = vm::Debug::default();

    // Collect the prototypes of entities that may be referred to in code.
    for (&name, item) in engine.iter() {
        match *item {
            Item::Native(api, arity, variadic) => {
                assets.api.insert(name, api);
                prototypes.insert(name, ssa::Prototype::Native { arity, variadic });
            }
            Item::Member(get, set) => {
                if let Some(get) = get { assets.get.insert(name, get); }
                if let Some(set) = set { assets.set.insert(name, set); }
                prototypes.insert(name, ssa::Prototype::Member);
            }
        }
    }
    for (id, &project::Script { name, .. }) in game.scripts.iter().enumerate() {
        let id = id as i32;
        let name = Symbol::intern(name);
        prototypes.insert(name, ssa::Prototype::Script { id });
        debug.scripts.push(name);
    }
    for &project::Object { name, .. } in game.objects.iter() {
        let name = Symbol::intern(name);
        debug.objects.push(name);
    }

    let mut total_errors = 0;

    // Compile scripts.
    let resources = Iterator::zip(debug.scripts.iter(), game.scripts.iter());
    for (id, (&script, &project::Script { body, .. })) in resources.enumerate() {
        let function = Function::Script { id: id as i32 };
        let name = FunctionDisplay::Script { script };
        let (code, locations, errors) = compile_program(&prototypes, name, body, errors());
        assets.code.insert(function, code);
        debug.locations.insert(function, locations);
        total_errors += errors;
    }

    // Compile object events.
    let resources = Iterator::zip(debug.objects.iter(), game.objects.iter());
    for (object_index, (&object, &project::Object { ref events, .. })) in resources.enumerate() {
        let object_index = object_index as i32;
        for &project::Event { event_type, event_kind, ref actions } in events {
            let function = Function::Event { object_index, event_type, event_kind };
            let event_kind = EventDisplay::from_debug(&debug, event_type, event_kind);
            let name = FunctionDisplay::Event { object, event_type, event_kind };
            let (code, locations, errors) = compile_event(&prototypes, name, actions, errors());
            assets.code.insert(function, code);
            debug.locations.insert(function, locations);
            total_errors += errors;
        }
    }

    if total_errors > 0 {
        return Err(total_errors);
    }

    Ok((assets, debug))
}

fn compile_program<E: io::Write + 'static>(
    prototypes: &HashMap<Symbol, ssa::Prototype>,
    name: FunctionDisplay,
    code: &[u8],
    errors: E,
) -> (code::Function, vm::Locations, u32) {
    let lines = Lines::from_code(code);
    let mut errors = ErrorPrinter::new(name, &lines, errors);
    let program = Parser::new(Lexer::new(code, 0), &mut errors).parse_program();
    let program = { front::Codegen::new(&prototypes, &mut errors).compile_program(&program) };
    let (code, locations) = back::Codegen::new(prototypes).compile(&program);
    let count = errors.count;
    (code, vm::Locations { locations, lines }, count)
}

fn compile_event<E: io::Write + 'static>(
    prototypes: &HashMap<Symbol, ssa::Prototype>,
    name: FunctionDisplay,
    actions: &[project::Action<'_>],
    errors: E,
) -> (code::Function, vm::Locations, u32) {
    let lines = Lines::from_actions(actions);
    let mut errors = ErrorPrinter::new(name, &lines, errors);
    let program = ActionParser::new(actions.iter(), &mut errors).parse_event();
    let program = front::Codegen::new(&prototypes, &mut errors).compile_event(&program);
    let (code, locations) = back::Codegen::new(prototypes).compile(&program);
    let count = errors.count;
    (code, vm::Locations { locations, lines }, count)
}

pub struct ErrorPrinter<'a, W: ?Sized = dyn io::Write> {
    pub name: FunctionDisplay,
    pub lines: &'a Lines,
    pub count: u32,
    pub write: W,
}

pub enum FunctionDisplay {
    Event { object: Symbol, event_type: u32, event_kind: EventDisplay },
    Script { script: Symbol },
}

#[derive(Copy, Clone)]
pub enum EventDisplay {
    Id(i32),
    Name(Symbol),
}

impl<'a> ErrorPrinter<'a> {
    pub fn new<W: io::Write>(name: FunctionDisplay, lines: &'a Lines, write: W) ->
        ErrorPrinter<'a, W>
    {
        ErrorPrinter { name, lines, count: 0, write }
    }

    pub fn from_debug<W: io::Write>(debug: &vm::Debug, function: Function, write: W) ->
        ErrorPrinter<W>
    {
        let name = match function {
            Function::Event { object_index, event_type, event_kind } => {
                let object = debug.objects[object_index as usize];
                let event_kind = EventDisplay::from_debug(debug, event_type, event_kind);
                FunctionDisplay::Event { object, event_type, event_kind }
            }
            Function::Script { id } => {
                let script = debug.scripts[id as usize];
                FunctionDisplay::Script { script }
            }
        };

        let lines = &debug.locations[&function].lines;

        ErrorPrinter::new(name, lines, write)
    }

    pub fn error(&mut self, span: Span, message: fmt::Arguments<'_>) {
        let Position { action, argument, line, column } = self.lines.get_position(span.low);
        let _ = write!(self.write, "error in {}", self.name);
        if let Some(action) = action {
            let _ = write!(self.write, ", action {}", action);
        }
        if let (Some(argument), None) = (argument, line) {
            let _ = write!(self.write, ", argument {}", argument);
        }
        if let Some(line) = line {
            let _ = write!(self.write, ":{}", line);
        }
        if let Some(column) = column {
            let _ = write!(self.write, ":{}", column);
        }
        let _ = writeln!(self.write, ": {}", message);
        self.count += 1;
    }
}

impl EventDisplay {
    fn from_debug(_: &vm::Debug, event_type: u32, event_kind: i32) -> EventDisplay {
        match event_type {
            _ => EventDisplay::Id(event_kind),
        }
    }
}

impl fmt::Display for FunctionDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FunctionDisplay::Event { object, event_type, event_kind } =>
                display_event(object, event_type, event_kind, f),
            FunctionDisplay::Script { script } => write!(f, "script {}", script),
        }
    }
}

fn display_event(
    object: Symbol, event_type: u32, event_kind: EventDisplay, f: &mut fmt::Formatter<'_>
) -> fmt::Result {
    write!(f, "event {}", event_type)?;
    match event_kind {
        EventDisplay::Id(id) => write!(f, "({})", id)?,
        EventDisplay::Name(name) => write!(f, "({})", name)?,
    }
    write!(f, " for object {}", object)?;
    Ok(())
}
