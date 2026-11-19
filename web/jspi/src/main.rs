//! Makes the threaded web module's threads work as instances on one page (web/single_file.py
//! --threads): a page that isn't cross-origin isolated may make shared memory but not hand it
//! to a worker, and may not block in `memory.atomic.wait`.
//!
//!     cargo run --release --manifest-path web/jspi/Cargo.toml -- IN.wasm OUT.wasm
//!
//! Every `memory.atomic.wait32/64` becomes a call that, while the instance's exported
//! `horizon_coroutine` global is set (web/horizon.js sets it in a thread's instance, and in the
//! page's while one of its event handlers runs in a promising call), calls the import
//! `horizon_jspi.wait32/64`, a `WebAssembly.Suspending` that resolves on a notify or the
//! timeout. Otherwise it returns "timed out" at once: a page may not block, and its waits loop.
//! Every `memory.atomic.notify` calls `horizon_jspi.notify`, which wakes those waits.
//!
//! OUT.entries.json lists the exported functions that return nothing: the page may run those
//! in a promising call, whose result it doesn't need.
use std::collections::HashMap;
use std::process::ExitCode;
use walrus::ir::{BinaryOp, Instr, InstrSeqId, Value};
use walrus::{
    ConstExpr, ExportItem, FunctionBuilder, FunctionId, InstrSeqBuilder, LocalId, Module, ValType,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Wait32,
    Wait64,
    Notify,
}

struct Imports {
    coroutine: walrus::GlobalId,
    wait32: FunctionId,
    wait64: FunctionId,
    notify: FunctionId,
}

/// The call that replaces one instruction: same operands, same result.
fn shim(m: &mut Module, imports: &Imports, kind: Kind, offset: u64) -> FunctionId {
    let params: &[ValType] = match kind {
        Kind::Wait32 => &[ValType::I32, ValType::I32, ValType::I64],
        Kind::Wait64 => &[ValType::I32, ValType::I64, ValType::I64],
        Kind::Notify => &[ValType::I32, ValType::I32],
    };
    let mut f = FunctionBuilder::new(&mut m.types, params, &[ValType::I32]);
    let locals: Vec<LocalId> = params.iter().map(|ty| m.locals.add(*ty)).collect();
    let address = |b: &mut InstrSeqBuilder| {
        b.local_get(locals[0]);
        if offset != 0 {
            b.i32_const(offset as i32).binop(BinaryOp::I32Add);
        }
    };
    let ty = walrus::ir::InstrSeqType::new(&mut m.types, &[], &[ValType::I32]);
    let mut body = f.func_body();
    match kind {
        Kind::Notify => {
            address(&mut body);
            body.local_get(locals[1]).call(imports.notify);
        }
        Kind::Wait32 | Kind::Wait64 => {
            let wide = kind == Kind::Wait64;
            let suspend = if wide { imports.wait64 } else { imports.wait32 };
            body.global_get(imports.coroutine).if_else(
                ty,
                |then| {
                    address(then);
                    then.local_get(locals[1]).local_get(locals[2]).call(suspend);
                },
                |otherwise| {
                    otherwise.i32_const(2);
                },
            );
        }
    }
    f.finish(locals, &mut m.funcs)
}

fn rewrite(m: &mut Module) -> Result<usize, String> {
    let coroutine = m
        .globals
        .add_local(ValType::I32, true, false, ConstExpr::Value(Value::I32(0)));
    m.exports.add("horizon_coroutine", coroutine);
    let wait32 = m
        .types
        .add(&[ValType::I32, ValType::I32, ValType::I64], &[ValType::I32]);
    let wait64 = m
        .types
        .add(&[ValType::I32, ValType::I64, ValType::I64], &[ValType::I32]);
    let notify = m.types.add(&[ValType::I32, ValType::I32], &[ValType::I32]);
    let imports = Imports {
        coroutine,
        wait32: m.add_import_func("horizon_jspi", "wait32", wait32).0,
        wait64: m.add_import_func("horizon_jspi", "wait64", wait64).0,
        notify: m.add_import_func("horizon_jspi", "notify", notify).0,
    };

    // Every site first: the shims add functions.
    let mut sites: Vec<(FunctionId, InstrSeqId, usize, Kind, u64)> = Vec::new();
    for (id, func) in m.funcs.iter_local() {
        let mut seqs = vec![func.entry_block()];
        while let Some(seq) = seqs.pop() {
            for (at, (instr, _)) in func.block(seq).instrs.iter().enumerate() {
                match instr {
                    Instr::AtomicWait(w) => {
                        let kind = if w.sixty_four {
                            Kind::Wait64
                        } else {
                            Kind::Wait32
                        };
                        sites.push((id, seq, at, kind, w.arg.offset));
                    }
                    Instr::AtomicNotify(n) => sites.push((id, seq, at, Kind::Notify, n.arg.offset)),
                    Instr::Block(b) => seqs.push(b.seq),
                    Instr::Loop(l) => seqs.push(l.seq),
                    Instr::IfElse(e) => seqs.extend([e.consequent, e.alternative]),
                    Instr::Try(t) => seqs.push(t.seq),
                    Instr::TryTable(t) => seqs.push(t.seq),
                    _ => {}
                }
            }
        }
    }
    let mut shims = HashMap::new();
    for &(id, seq, at, kind, offset) in &sites {
        let func = *shims
            .entry((kind, offset))
            .or_insert_with(|| shim(m, &imports, kind, offset));
        let local = m.funcs.get_mut(id).kind.unwrap_local_mut();
        local.block_mut(seq).instrs[at].0 = Instr::Call(walrus::ir::Call { func });
    }
    Ok(sites.len())
}

/// The exported functions that return nothing, as a JSON list of names.
fn entries(m: &Module) -> String {
    let names: Vec<String> = m
        .exports
        .iter()
        .filter(|e| match e.item {
            ExportItem::Function(f) => m.types.get(m.funcs.get(f).ty()).results().is_empty(),
            _ => false,
        })
        .map(|e| format!("{:?}", e.name))
        .collect();
    format!("[{}]", names.join(","))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: horizon-web-jspi IN.wasm OUT.wasm");
        return ExitCode::FAILURE;
    };
    let result = Module::from_file(input)
        .map_err(|e| format!("{input}: {e}"))
        .and_then(|mut m| {
            let sites = rewrite(&mut m)?;
            m.emit_wasm_file(output)
                .map_err(|e| format!("{output}: {e}"))?;
            let list = format!("{output}.entries.json");
            std::fs::write(&list, entries(&m)).map_err(|e| format!("{list}: {e}"))?;
            Ok(sites)
        });
    match result {
        Ok(sites) => {
            println!("{output}: {sites} waits and notifies suspend on the page");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("horizon-web-jspi: {e}");
            ExitCode::FAILURE
        }
    }
}
