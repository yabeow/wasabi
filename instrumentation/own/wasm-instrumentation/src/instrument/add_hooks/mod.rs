use ast::{self, BlockType, GlobalType, Idx, InstrType, Mutability, Val, ValType::*};
use ast::highlevel::{Function, Global, GlobalOp::*, Instr, Instr::*, LocalOp::*, Module};
use serde_json;
use super::block_stack::{BlockStack, BlockStackElement};
use super::convert_i64::convert_i64_instr;
use super::hook_map::HookMap;
use super::static_info::*;
use super::type_stack::TypeStack;

/// instruments every instruction in Jalangi-style with a callback that takes inputs, outputs, and
/// other relevant information.
pub fn add_hooks(module: &mut Module) -> Option<String> {
    /*
     * make sure every function and table is exported,
     * needed for Wasabi runtime to resolve table indices to function indices
     */
    for table in &mut module.tables {
        if let None = table.export {
            table.export = Some("__wasabi_table".into());
        }
    }
    for (fidx, function) in module.functions() {
        if let None = function.export {
            function.export = Some(format!("__wasabi_function_{}", fidx.0));
        }
    }

    // NOTE must be after exporting table and function, so that their export names are in the static info object
    let mut module_info: ModuleInfo = (&*module).into();
    let mut hooks = HookMap::new(&module);

    // add global for start, set to false on the first execution of the start function
    let start_not_executed_global = {
        module.globals.push(Global {
            type_: GlobalType(I32, Mutability::Mut),
            init: Some(vec![Const(Val::I32(1)), End]),
            import: None,
            export: None,
        });
        module.globals.len() - 1
    };

    for (fidx, function) in module.functions() {
        // only instrument non-imported functions
        if function.code.is_none() {
            continue;
        }

        // move body out of function, so that function is not borrowed during iteration over the original body
        let original_body = {
            let dummy_body = Vec::new();
            ::std::mem::replace(&mut function.code.as_mut().unwrap().body, dummy_body)
        };

        // allocate new instrumented body (i.e., do not modify in-place), since there are too many insertions anyway
        // there are at least 3 new instructions per original one (2 const for location + 1 hook call)
        let mut instrumented_body = Vec::with_capacity(4 * original_body.len());

        // for branch target resolution (i.e., relative labels -> instruction locations)
        let mut block_stack = BlockStack::new(&original_body);
        // for drop/select monomorphization (cannot determine their input types only from instruction, but need this additional type information)
        let mut type_stack = TypeStack::new();

        // execute start hook before anything else...
        if module_info.start == Some(fidx) {
            instrumented_body.extend_from_slice(&[
                Global(GetGlobal, start_not_executed_global.into()),
                // ...(if this is the start function and it hasn't run yet)
                If(BlockType(None)),
                Const(Val::I32(0)),
                Global(SetGlobal, start_not_executed_global.into()),
                fidx.into(),
                Const(Val::I32(-1)),
                hooks.start(),
                End,
            ]);
        }

        // add function_begin hook...
        instrumented_body.extend_from_slice(&[
            fidx.into(),
            // ...which does not correspond to any instruction, so take -1 as instruction index
            Const(Val::I32(-1)),
            hooks.begin_function()
        ]);

        // check for implicit return now, since body gets consumed below
        let implicit_return = !original_body.ends_with(&[Return, End]);

        for (iidx, instr) in original_body.into_iter().enumerate() {
            let iidx: Idx<Instr> = iidx.into();
            let location = (fidx.into(), iidx.into());

            /*
             * add calls to hooks, typical instructions inserted for (not necessarily in this order if that saves us a local or so):
             * 1. duplicate instruction inputs via temporary locals
             * 2. call original instruction (except for in a few cases, where the hook is inserted before)
             * 3. duplicate instruction results via temporary locals
             * 4. push instruction location (function + instr index)
             * 5. call hook
             */
            match instr {
                // size optimization: replace nop fully with hook
                Nop => instrumented_body.extend_from_slice(&[
                    location.0,
                    location.1,
                    hooks.instr(&instr, &[])
                ]),
                // hook must come before unreachable instruction, otherwise it prevents hook from being called
                Unreachable => instrumented_body.extend_from_slice(&[
                    location.0,
                    location.1,
                    hooks.instr(&instr, &[]),
                    instr
                ]),


                /* Control Instructions: Blocks */

                Block(block_ty) => {
                    block_stack.begin_block(iidx);
                    type_stack.begin(block_ty);

                    instrumented_body.extend_from_slice(&[
                        instr,
                        location.0,
                        location.1,
                        hooks.begin_block(),
                    ]);
                }
                Loop(block_ty) => {
                    block_stack.begin_loop(iidx);
                    type_stack.begin(block_ty);

                    instrumented_body.extend_from_slice(&[
                        instr,
                        location.0,
                        location.1,
                        hooks.begin_loop(),
                    ]);
                }
                If(block_ty) => {
                    block_stack.begin_if(iidx);
                    type_stack.begin(block_ty);

                    let condition_tmp = function.add_fresh_local(I32);

                    instrumented_body.extend_from_slice(&[
                        // if_ hook for the condition (always executed on either branch)
                        Local(TeeLocal, condition_tmp),
                        location.0.clone(),
                        location.1.clone(),
                        Local(GetLocal, condition_tmp),
                        hooks.instr(&instr, &[]),
                        // actual if block start
                        instr,
                        // begin hook (not executed when condition implies else branch)
                        location.0,
                        location.1,
                        hooks.begin_if(),
                    ]);
                }
                Else => {
                    let if_block = block_stack.else_();
                    let begin_if = if let BlockStackElement::If { begin_if, .. } = if_block {
                        begin_if
                    } else {
                        unreachable!()
                    };

                    type_stack.else_();

                    instrumented_body.extend_from_slice(&[
                        location.0.clone(),
                        location.1.clone(),
                        begin_if.into(),
                        hooks.end(&if_block),
                        instr,
                        location.0,
                        location.1,
                        begin_if.into(),
                        hooks.begin_else(),
                    ]);
                }
                End => {
                    let block = block_stack.end();
                    type_stack.end();

                    instrumented_body.extend_from_slice(&[
                        location.0,
                        location.1,
                    ]);
                    // arguments for end hook
                    instrumented_body.append(&mut match block {
                        BlockStackElement::Function { .. } => vec![],
                        BlockStackElement::Block { begin, .. } | BlockStackElement::Loop { begin, .. } | BlockStackElement::If { begin_if: begin, .. } => vec![begin.into()],
                        BlockStackElement::Else { begin_if, begin_else, .. } => vec![begin_if.into(), begin_else.into()]
                    });
                    instrumented_body.extend_from_slice(&[
                        hooks.end(&block),
                        instr
                    ]);
                }


                /* Control Instructions: Branches/Breaks */
                // NOTE hooks must come before instr

                Br(target_label) => instrumented_body.extend_from_slice(&[
                    location.0,
                    location.1,
                    target_label.into(),
                    block_stack.br_target(target_label).into(),
                    hooks.instr(&instr, &[]),
                    instr
                ]),
                BrIf(target_label) => {
                    type_stack.instr(&InstrType::new(&[I32], &[]));

                    let condition_tmp = function.add_fresh_local(I32);

                    instrumented_body.extend_from_slice(&[
                        Local(TeeLocal, condition_tmp),
                        location.0,
                        location.1,
                        target_label.into(),
                        block_stack.br_target(target_label).into(),
                        Local(GetLocal, condition_tmp),
                        hooks.instr(&instr, &[]),
                        instr
                    ]);
                }
                BrTable(ref target_table, default_target) => {
                    type_stack.instr(&InstrType::new(&[I32], &[]));

                    // each br_table instruction gets its own entry in the static info object
                    // that maps table index to label and location
                    module_info.br_tables.push(BrTableInfo::from_br_table(target_table, default_target, &block_stack, fidx));

                    let target_idx_tmp = function.add_fresh_local(I32);

                    instrumented_body.extend_from_slice(&[
                        Local(TeeLocal, target_idx_tmp),
                        location.0,
                        location.1,
                        Const(Val::I32((module_info.br_tables.len() - 1) as i32)),
                        Local(GetLocal, target_idx_tmp),
                        hooks.instr(&instr, &[]),
                        instr.clone()
                    ]);
                }


                /* Control Instructions: Calls & Returns */

                Return => {
                    type_stack.instr(&InstrType::new(&[], &function.type_.results));

                    let result_tys = &function.type_.results.clone();
                    let result_tmps = function.add_fresh_locals(result_tys);

                    instrumented_body.append(&mut save_stack_to_locals(&result_tmps));
                    instrumented_body.extend_from_slice(&[
                        location.0,
                        location.1,
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&result_tmps, &function));
                    instrumented_body.extend_from_slice(&[
                        hooks.instr(&instr, result_tys),
                        instr,
                    ]);
                }
                Call(target_func_idx) => {
                    let ref func_ty = module_info.functions[target_func_idx.0].type_;
                    type_stack.instr(&func_ty.into());

                    /* pre call hook */

                    let arg_tmps = function.add_fresh_locals(&func_ty.params);

                    instrumented_body.append(&mut save_stack_to_locals(&arg_tmps));
                    instrumented_body.extend_from_slice(&[
                        location.0.clone(),
                        location.1.clone(),
                        target_func_idx.into(),
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&arg_tmps, &function));
                    instrumented_body.extend_from_slice(&[
                        hooks.instr(&instr, &func_ty.params),
                        instr,
                    ]);

                    /* post call hook */

                    let result_tmps = function.add_fresh_locals(&func_ty.results);

                    instrumented_body.append(&mut save_stack_to_locals(&result_tmps));
                    instrumented_body.extend_from_slice(&[
                        location.0,
                        location.1,
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&result_tmps, &function));
                    instrumented_body.push(hooks.call_post(&func_ty.results));
                }
                CallIndirect(ref func_ty, _ /* table idx == 0 in WASM version 1 */) => {
                    type_stack.instr(&func_ty.into());

                    /* pre call hook */

                    let target_table_idx_tmp = function.add_fresh_local(I32);
                    let arg_tmps = function.add_fresh_locals(&func_ty.params);

                    instrumented_body.push(Local(SetLocal, target_table_idx_tmp));
                    instrumented_body.append(&mut save_stack_to_locals(&arg_tmps));
                    instrumented_body.extend_from_slice(&[
                        Local(GetLocal, target_table_idx_tmp),
                        location.0.clone(),
                        location.1.clone(),
                        Local(GetLocal, target_table_idx_tmp),
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&arg_tmps, &function));
                    instrumented_body.extend_from_slice(&[
                        hooks.instr(&instr, &func_ty.params),
                        instr.clone(),
                    ]);

                    /* post call hook */

                    let result_tmps = function.add_fresh_locals(&func_ty.results);

                    instrumented_body.append(&mut save_stack_to_locals(&result_tmps));
                    instrumented_body.extend_from_slice(&[
                        location.0,
                        location.1,
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&result_tmps, &function));
                    instrumented_body.push(hooks.call_post(&func_ty.results));
                }


                /* Parametric Instructions */

                Drop => {
                    let ty = type_stack.pop_val();

                    let tmp = function.add_fresh_local(ty);

                    instrumented_body.extend_from_slice(&[
                        Local(SetLocal, tmp),
                        location.0,
                        location.1,
                    ]);
                    instrumented_body.append(&mut convert_i64_instr(Local(GetLocal, tmp), ty));
                    instrumented_body.push(hooks.instr(&instr, &[ty]));
                }
                Select => {
                    assert_eq!(type_stack.pop_val(), I32, "select condition should be i32");
                    let ty = type_stack.pop_val();
                    assert_eq!(type_stack.pop_val(), ty, "select arguments should have same type");
                    type_stack.push_val(ty);

                    let condition_tmp = function.add_fresh_local(I32);
                    let arg_tmps = function.add_fresh_locals(&[ty, ty]);

                    instrumented_body.append(&mut save_stack_to_locals(&[arg_tmps[0], arg_tmps[1], condition_tmp]));
                    instrumented_body.extend_from_slice(&[
                        instr.clone(),
                        location.0,
                        location.1,
                        Local(GetLocal, condition_tmp),
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&arg_tmps, &function));
                    instrumented_body.push(hooks.instr(&instr, &[ty, ty]));
                }


                /* Variable Instructions */

                Local(op, local_idx) => {
                    let local_ty = function.local_type(local_idx);

                    type_stack.instr(&op.to_type(local_ty));

                    instrumented_body.extend_from_slice(&[
                        instr.clone(),
                        location.0,
                        location.1,
                        local_idx.into(),
                    ]);
                    instrumented_body.append(&mut convert_i64_instr(Local(GetLocal, local_idx), local_ty));
                    instrumented_body.push(hooks.instr(&instr, &[local_ty]));
                }
                Global(op, global_idx) => {
                    let global_ty = module_info.globals[global_idx.0];

                    type_stack.instr(&op.to_type(global_ty));

                    instrumented_body.extend_from_slice(&[
                        instr.clone(),
                        location.0,
                        location.1,
                        global_idx.into(),
                    ]);
                    instrumented_body.append(&mut convert_i64_instr(Global(GetGlobal, global_idx), global_ty));
                    instrumented_body.push(hooks.instr(&instr, &[global_ty]));
                }


                /* Memory Instructions */

                MemorySize(_ /* memory idx == 0 in WASM version 1 */) => {
                    type_stack.instr(&instr.to_type().unwrap());

                    instrumented_body.extend_from_slice(&[
                        instr.clone(),
                        location.0,
                        location.1,
                        // optimization: just call memory_size again instead of duplicating result into local
                        instr.clone(),
                        hooks.instr(&instr, &[])
                    ]);
                }
                MemoryGrow(_ /* memory idx == 0 in WASM version 1 */) => {
                    type_stack.instr(&instr.to_type().unwrap());

                    let input_tmp = function.add_fresh_local(I32);
                    let result_tmp = function.add_fresh_local(I32);

                    instrumented_body.extend_from_slice(&[
                        Local(TeeLocal, input_tmp),
                        instr.clone(),
                        Local(TeeLocal, result_tmp),
                        location.0,
                        location.1,
                        Local(GetLocal, input_tmp),
                        Local(GetLocal, result_tmp),
                        hooks.instr(&instr, &[])
                    ]);
                }

                // rest are "grouped instructions", i.e., where many instructions can be handled in a similar manner
                Load(op, memarg) => {
                    let ty = op.to_type();
                    type_stack.instr(&ty);

                    let addr_tmp = function.add_fresh_local(I32);
                    let value_tmp = function.add_fresh_local(ty.results[0]);

                    instrumented_body.extend_from_slice(&[
                        Local(TeeLocal, addr_tmp),
                        instr.clone(),
                        Local(TeeLocal, value_tmp),
                        location.0,
                        location.1,
                        Const(Val::I32(memarg.offset as i32)),
                        Const(Val::I32(memarg.alignment as i32)),
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&[addr_tmp, value_tmp], &function));
                    instrumented_body.push(hooks.instr(&instr, &[]));
                }
                Store(op, memarg) => {
                    let ty = op.to_type();
                    type_stack.instr(&ty);

                    let addr_tmp = function.add_fresh_local(I32);
                    let value_tmp = function.add_fresh_local(ty.inputs[0]);

                    instrumented_body.append(&mut save_stack_to_locals(&[addr_tmp, value_tmp]));
                    instrumented_body.extend_from_slice(&[
                        instr.clone(),
                        location.0,
                        location.1,
                        Const(Val::I32(memarg.offset as i32)),
                        Const(Val::I32(memarg.alignment as i32)),
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(&[addr_tmp, value_tmp], &function));
                    instrumented_body.push(hooks.instr(&instr, &[]));
                }


                /* Numeric Instructions */

                Const(val) => {
                    type_stack.instr(&instr.to_type().unwrap());

                    instrumented_body.extend_from_slice(&[
                        instr.clone(),
                        location.0,
                        location.1,
                    ]);
                    // optimization: just call T.const again, instead of duplicating result into local
                    instrumented_body.append(&mut convert_i64_instr(instr.clone(), val.to_type()));
                    instrumented_body.push(hooks.instr(&instr, &[]));
                }
                Numeric(op) => {
                    let ty = op.to_type();
                    type_stack.instr(&ty);

                    let input_tmps = function.add_fresh_locals(&ty.inputs);
                    let result_tmps = function.add_fresh_locals(&ty.results);

                    instrumented_body.append(&mut save_stack_to_locals(&input_tmps));
                    instrumented_body.push(instr.clone());
                    instrumented_body.append(&mut save_stack_to_locals(&result_tmps));
                    instrumented_body.extend_from_slice(&[
                        location.0,
                        location.1,
                    ]);
                    instrumented_body.append(&mut restore_locals_with_i64_handling(
                        &[input_tmps, result_tmps].concat(),
                        &function));
                    instrumented_body.push(hooks.instr(&instr, &[]));
                }
            }
        }

        // add return hook, if function has an implicit return
        // (can be distinguished from actual returns in analysis because of -1 as instr location)
        if implicit_return {
            let result_tys = &function.type_.results.clone();
            let result_tmps = function.add_fresh_locals(result_tys);

            assert_eq!(instrumented_body.pop(), Some(End));
            instrumented_body.append(&mut save_stack_to_locals(&result_tmps));
            instrumented_body.extend_from_slice(&[
                fidx.into(),
                Const(Val::I32(-1)),
            ]);
            instrumented_body.append(&mut restore_locals_with_i64_handling(&result_tmps, &function));
            instrumented_body.extend_from_slice(&[
                hooks.instr(&Return, result_tys),
                End,
            ]);
        }

        // finally, switch dummy body out against instrumented body
        ::std::mem::replace(&mut function.code.as_mut().unwrap().body, instrumented_body);
    }

    // actually add the hooks to module and check that inserted Idx is the one on the Hook struct
    let hooks = hooks.finish();
    let mut js_hooks = Vec::new();
    for hook in hooks {
        js_hooks.push(hook.js);
        assert_eq!(hook.idx, module.functions.len().into(), "have other functions been inserted into the module since starting collection of hooks?");
        module.functions.push(hook.wasm);
    }

    Some(generate_js(module_info, &js_hooks))
}

/// helper function to save top locals.len() values into locals with the given index
/// types of locals must match stack, not enforced by this function!
fn save_stack_to_locals(locals: &[Idx<ast::Local>]) -> Vec<Instr> {
    let mut instrs = Vec::new();
    // copy stack values into locals
    for &local in locals.iter().skip(1).rev() {
        instrs.push(Local(SetLocal, local));
    }
    // optimization: for first local on the stack / last one saved use tee_local instead of set_local + get_local
    for &local in locals.iter().next() {
        instrs.push(Local(TeeLocal, local));
    }
    // and restore (saving has removed them from the stack)
    for &local in locals.iter().skip(1) {
        instrs.push(Local(GetLocal, local));
    }
    return instrs;
}

/// function is necessary to get the types of the locals
fn restore_locals_with_i64_handling(locals: &[Idx<ast::Local>], function: &Function) -> Vec<Instr> {
    let mut instrs = Vec::new();
    for &local in locals {
        instrs.append(&mut convert_i64_instr(Local(GetLocal, local), function.local_type(local)));
    }
    return instrs;
}

/// convenience to hand (function/instr/local/global) indices to hooks
impl<T> Into<Instr> for Idx<T> {
    fn into(self) -> Instr {
        Const(Val::I32(self.0 as i32))
    }
}

fn generate_js(module_info: ModuleInfo, hooks: &[String]) -> String {
    format!(r#"/*
 * Auto-generated from WASM module to-analyze.
 * DO NOT EDIT.
 */

Wasabi.module.info = {};

Wasabi.module.lowlevelHooks = {{
    {}
}};
"#,
            serde_json::to_string_pretty(&module_info).unwrap(),
            hooks.iter().flat_map(|s| s.split("\n")).collect::<Vec<&str>>().join("\n    "))
}