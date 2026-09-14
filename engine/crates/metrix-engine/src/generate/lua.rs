//! The default tier: embedded Lua, one VM per worker thread, reused across requests.
//!
//! No process boundary, no serialization, no IPC (design-engine §7.2). It is fast
//! enough to sit on the hot path at the rates in scope, and it keeps generation logic
//! inside the plan's directory rather than in a separate deployable.
//!
//! **One VM per thread, in a thread local.** A `Lua` is not `Sync`, and putting one
//! behind a mutex would serialise every worker through a single interpreter — which
//! is exactly the generator-limited run the calibration exists to detect. Generation
//! never awaits, so the borrow cannot cross a yield point and a thread local is
//! enough: no lock, no channel, no VM per request.
//!
//! **Sandboxed** (§7.2): no `os.execute`, no network, no `require`. The standard
//! libraries are chosen rather than pruned — `io`, `package` and `debug` are never
//! loaded, so there is no window between opening them and taking them away. B3.7 adds
//! reading a corpus, which is a controlled call and not `io`.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use mlua::{Lua, LuaOptions, StdLib, Table, Value as LuaValue};

use super::{Built, Context, require};
use crate::random::Rng;

thread_local! {
    /// This thread's interpreters, one per script, built on first use.
    static VMS: RefCell<Vec<Option<Prepared>>> = const { RefCell::new(Vec::new()) };
}

struct Prepared {
    lua: Lua,
    entry: mlua::Function,
    /// The stream `ctx.rng` draws from. Owned by the interpreter rather than passed
    /// in, because a Lua function has to outlive the call that built it — so the
    /// iteration's stream is swapped into this cell around each call instead.
    rng: Rc<RefCell<Rng>>,
}

/// One `.lua` file, read at load and compiled per thread on first use.
pub(crate) struct Script {
    /// Its position in the plan's generator list, which is its slot in the thread's
    /// interpreter table.
    slot: usize,
    name: Arc<str>,
    source: Arc<str>,
    entry: Arc<str>,
}

/// How many scripts have been declared, so each gets a distinct thread-local slot.
static SLOTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Script {
    pub fn load(at: &str, root: &Path, file: &Path, entry: &str) -> Result<Self, String> {
        let path = super::in_bundle(at, root, file)?;
        require(
            path.extension().and_then(|e| e.to_str()) == Some("lua"),
            &format!("{at}/file: expected a .lua file, got {file:?}"),
        )?;
        let source = std::fs::read_to_string(&path)
            .map_err(|error| format!("{at}/file: cannot read {file:?} — {error}"))?;
        require(
            !entry.is_empty(),
            &format!("{at}/entry: an empty function name"),
        )?;
        let script = Self {
            slot: SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            name: file.display().to_string().into(),
            source: source.into(),
            entry: entry.into(),
        };
        // Compiled once here so a syntax error or a missing entry function is a
        // load-time error rather than a run that starts and immediately fails
        // everything. The VM is thrown away; the workers build their own.
        script.prepare().map_err(|error| format!("{at}: {error}"))?;
        Ok(script)
    }

    /// Build this thread's interpreter for this script.
    fn prepare(&self) -> Result<Prepared, String> {
        // Chosen rather than pruned: `io`, `package` and `debug` are never opened.
        let lua = Lua::new_with(
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::OS,
            LuaOptions::default(),
        )
        .map_err(|error| format!("cannot start Lua — {error}"))?;
        sandbox(&lua).map_err(|error| format!("cannot sandbox Lua — {error}"))?;
        lua.load(&*self.source)
            .set_name(&*self.name)
            .exec()
            .map_err(|error| format!("{error}"))?;
        let entry: mlua::Function = lua
            .globals()
            .get(&*self.entry)
            .map_err(|_| format!("the script defines no function named {:?}", self.entry))?;
        Ok(Prepared {
            rng: Rc::new(RefCell::new(Rng::seeded(0, 0))),
            lua,
            entry,
        })
    }

    /// Run the hook for one request.
    pub fn call(&self, context: &mut Context<'_>) -> Result<Built, String> {
        VMS.with(|vms| {
            let mut vms = vms.borrow_mut();
            if vms.len() <= self.slot {
                vms.resize_with(self.slot + 1, || None);
            }
            if vms[self.slot].is_none() {
                vms[self.slot] = Some(self.prepare()?);
            }
            let prepared = vms[self.slot].as_ref().expect("just built");
            // In, and back out: the draws a generator makes are part of the
            // iteration's stream, so a step after it does not re-use their values and
            // a replay of the plan produces the same sequence.
            *prepared.rng.borrow_mut() = context.rng.clone();
            let argument = to_lua(prepared, context)?;
            let returned: Result<LuaValue, _> = prepared.entry.call(argument);
            *context.rng = prepared.rng.borrow().clone();
            from_lua(&returned.map_err(|error| trim(&error.to_string()))?)
        })
    }
}

/// Take away what the standard libraries leave behind.
///
/// `os` is loaded for `time`, `clock` and `date`, which a generator stamping a
/// request reasonably wants. The rest of it reaches the machine, and `load` and its
/// relatives would let a plan compile source that was never read from the bundle.
fn sandbox(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in [
        "load",
        "loadstring",
        "dofile",
        "loadfile",
        "require",
        "collectgarbage",
    ] {
        globals.set(name, LuaValue::Nil)?;
    }
    if let Ok(os) = globals.get::<Table>("os") {
        for name in [
            "execute",
            "exit",
            "remove",
            "rename",
            "tmpname",
            "getenv",
            "setlocale",
        ] {
            os.set(name, LuaValue::Nil)?;
        }
    }
    Ok(())
}

/// The context, as the table the hook receives.
fn to_lua(prepared: &Prepared, context: &Context<'_>) -> Result<LuaValue, String> {
    let lua = &prepared.lua;
    let build = || -> mlua::Result<Table> {
        let table = lua.create_table()?;
        table.set("vu", context.vu)?;
        table.set("iteration", context.iteration)?;
        table.set("step", context.step)?;

        let vars = lua.create_table()?;
        for (name, value) in context.vars {
            vars.set(name.as_str(), value.as_str())?;
        }
        table.set("vars", vars)?;

        let rows = lua.create_table()?;
        for (dataset, fields) in &context.rows {
            let row = lua.create_table()?;
            for (column, value) in fields {
                row.set(*column, *value)?;
            }
            rows.set(*dataset, row)?;
        }
        table.set("rows", rows)?;

        let args = lua.create_table()?;
        for (name, value) in context.args {
            args.set(name.as_str(), value.as_str())?;
        }
        table.set("args", args)?;

        // The iteration's own stream. A generator calling `math.random` instead
        // would be outside the seed, and the run would not replay.
        table.set("rng", prepared.rng_table()?)?;
        Ok(table)
    };
    build().map(LuaValue::Table).map_err(|e| e.to_string())
}

impl Prepared {
    /// `ctx.rng:int(low, high)`, drawing from whatever stream is in the cell.
    ///
    /// Built per call rather than kept, because the table is handed to the script and
    /// a script that stashed it in a global would otherwise keep drawing from a later
    /// iteration's stream.
    fn rng_table(&self) -> mlua::Result<Table> {
        let table = self.lua.create_table()?;
        let stream = Rc::clone(&self.rng);
        table.set(
            "int",
            self.lua
                .create_function(move |_, (_self, low, high): (LuaValue, i64, i64)| {
                    Ok(stream.borrow_mut().in_range(low.min(high), low.max(high)))
                })?,
        )?;
        Ok(table)
    }
}

/// What the hook returned, as the parts of a request.
fn from_lua(value: &LuaValue) -> Result<Built, String> {
    let LuaValue::Table(table) = value else {
        return Err(format!(
            "the generator returned {} and a request is a table",
            value.type_name()
        ));
    };
    let mut built = Built::default();
    if let Ok(path) = table.get::<Option<String>>("path") {
        built.path = path;
    }
    built.query = pairs(table, "query")?;
    built.headers = pairs(table, "headers")?;
    match table.get::<LuaValue>("body") {
        Ok(LuaValue::String(body)) => built.body = Some(body.as_bytes().to_vec()),
        Ok(LuaValue::Nil) => {}
        Ok(other) => {
            return Err(format!(
                "the generator returned a {} body and a body is a string",
                other.type_name()
            ));
        }
        Err(error) => return Err(trim(&error.to_string())),
    }
    Ok(built)
}

/// A Lua table of strings, in the order Lua walks it.
fn pairs(table: &Table, field: &str) -> Result<Option<Vec<(String, String)>>, String> {
    match table.get::<LuaValue>(field) {
        Ok(LuaValue::Nil) => Ok(None),
        Ok(LuaValue::Table(inner)) => {
            let mut collected = Vec::new();
            for entry in inner.pairs::<LuaValue, LuaValue>() {
                let (key, value) = entry.map_err(|error| trim(&error.to_string()))?;
                let (Some(key), Some(value)) = (scalar(&key), scalar(&value)) else {
                    return Err(format!(
                        "the generator returned a {field} entry that is not a string"
                    ));
                };
                collected.push((key, value));
            }
            // Lua's own order is unspecified for a hash part, and a request whose
            // query parameters came out in a different order on each call would make
            // two identical runs look different in the event stream.
            collected.sort();
            Ok(Some(collected))
        }
        Ok(other) => Err(format!(
            "the generator returned a {} {field} and it has to be a table",
            other.type_name()
        )),
        Err(error) => Err(trim(&error.to_string())),
    }
}

fn scalar(value: &LuaValue) -> Option<String> {
    match value {
        LuaValue::String(text) => Some(text.to_string_lossy()),
        LuaValue::Integer(number) => Some(number.to_string()),
        LuaValue::Number(number) => Some(number.to_string()),
        LuaValue::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// One line of a Lua error, without the traceback.
///
/// The traceback names this file's own frames, which are not where the problem is;
/// the message and the script's line number are.
fn trim(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or(message)
        .trim()
        .trim_start_matches("runtime error: ")
        .to_owned()
}
