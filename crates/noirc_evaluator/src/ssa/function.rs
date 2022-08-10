use std::collections::HashMap;

use crate::environment::Environment;
use crate::errors::RuntimeError;

use acvm::acir::OPCODE;
use acvm::FieldElement;
use noirc_frontend::hir_def::expr::{HirCallExpression, HirIdent};
use noirc_frontend::hir_def::function::Parameters;
use noirc_frontend::hir_def::stmt::HirPattern;
use noirc_frontend::node_interner::FuncId;

use super::conditional::{AssumptionId, DecisionTree};
use super::mem::ArrayId;
use super::node::Node;
use super::{
    block::BlockId,
    code_gen::IRGenerator,
    context::SsaContext,
    node::{self, NodeId, ObjectType},
    ssa_form,
};

#[derive(Clone, Debug, PartialEq, Copy)]
pub struct FuncIndex(pub usize);

impl FuncIndex {
    pub fn new(idx: usize) -> FuncIndex {
        FuncIndex(idx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SsaFuncId {
    pub id: FuncId,
    pub template_id: usize,
}

#[derive(Clone, Debug)]
pub struct Template {
    //Indicates the len of the slices in the argument list of a 'template' function
    // ArrayId      = id of the slice
    // (usize, u32) = position of the slice in the argument list, length of the slice
    slice: HashMap<ArrayId, (usize, u32)>,
}

impl Template {
    pub fn new() -> Template {
        Template { slice: HashMap::new() }
    }

    pub fn push(&mut self, pos: usize, a: ArrayId, len: u32) {
        self.slice.insert(a, (pos, len));
    }

    pub fn matches(&self, template_args: &HashMap<usize, u32>) -> bool {
        for (pos, len) in self.slice.values() {
            if !template_args.contains_key(pos) || template_args[pos] != *len {
                return false;
            }
        }
        true
    }

    pub fn templatize(ctx: &SsaContext, arguments: &[NodeId]) -> HashMap<usize, u32> {
        let mut result = HashMap::new();
        for i in arguments.iter().enumerate() {
            if let ObjectType::Pointer(a) = ctx.get_object_type(*i.1) {
                let len = ctx.mem.len(a);
                result.insert(i.0, len);
            }
        }
        result
    }

    pub fn template_arguments(&self) -> HashMap<ArrayId, u32> {
        let mut result = HashMap::new();
        for i in &self.slice {
            result.insert(*i.0, i.1 .1);
        }
        result
    }
}

#[derive(Clone, Debug)]
pub struct SSAFunction {
    pub entry_block: BlockId,
    pub id: SsaFuncId,
    pub idx: FuncIndex,
    //signature:
    pub name: String,
    pub arguments: Vec<(NodeId, bool)>,
    pub result_types: Vec<ObjectType>,
    pub decision: DecisionTree,
    pub template: Template,
}

impl SSAFunction {
    pub fn new(
        func: FuncId,
        name: &str,
        block_id: BlockId,
        idx: FuncIndex,
        ctx: &SsaContext,
    ) -> SSAFunction {
        SSAFunction {
            entry_block: block_id,
            id: SsaFuncId { id: func, template_id: 0 },
            name: name.to_string(),
            arguments: Vec::new(),
            result_types: Vec::new(),
            decision: DecisionTree::new(ctx),
            idx,
            template: Template::new(),
        }
    }

    pub fn compile(&self, igen: &mut IRGenerator) -> Result<DecisionTree, RuntimeError> {
        let function_cfg = super::block::bfs(self.entry_block, None, &igen.context);
        super::block::compute_sub_dom(&mut igen.context, &function_cfg);
        //Optimisation
        super::optim::full_cse(&mut igen.context, self.entry_block)?;
        //Unrolling
        super::flatten::unroll_tree(&mut igen.context, self.entry_block)?;

        //reduce conditionals
        let mut decision = DecisionTree::new(&igen.context);
        decision.make_decision_tree(&mut igen.context, self.entry_block);
        decision.reduce(&mut igen.context, decision.root)?;

        super::optim::full_cse(&mut igen.context, self.entry_block)?;
        Ok(decision)
    }

    //generates an instruction for calling the function
    pub fn call(
        func_id: SsaFuncId,
        arguments: &[noirc_frontend::node_interner::ExprId],
        igen: &mut IRGenerator,
        env: &mut Environment,
    ) -> Result<Vec<NodeId>, RuntimeError> {
        let arguments = igen.codegen_expression_list(env, arguments);
        let call_instruction = igen.context.new_instruction(
            node::Operation::Call {
                func_id,
                arguments,
                returned_arrays: Vec::new(),
                predicate: AssumptionId::dummy(),
            },
            ObjectType::NotAnObject,
        )?;
        let func = igen.context.get_ssafunc(func_id).unwrap();
        let rtt = func.result_types.clone();
        let mut result = Vec::new();
        for i in rtt.iter().enumerate() {
            result.push(igen.context.new_instruction(
                node::Operation::Result { call_instruction, index: i.0 as u32 },
                *i.1,
            )?);
        }
        Ok(result)
    }

    pub fn get_mapped_value(
        var: Option<&NodeId>,
        ctx: &mut SsaContext,
        inline_map: &HashMap<NodeId, NodeId>,
        block_id: BlockId,
    ) -> NodeId {
        if let Some(&node_id) = var {
            if node_id == NodeId::dummy() {
                return node_id;
            }
            let mut my_const = None;
            let node_obj_opt = ctx.try_get_node(node_id);
            if let Some(node::NodeObj::Const(c)) = node_obj_opt {
                my_const = Some((c.get_value_field(), c.value_type));
            }
            if let Some(c) = my_const {
                ctx.get_or_create_const(c.0, c.1)
            } else if let Some(id) = inline_map.get(&node_id) {
                *id
            } else {
                ssa_form::get_current_value_in_block(ctx, node_id, block_id)
            }
        } else {
            NodeId::dummy()
        }
    }
}

//Returns the number of elements and their type, of the output result corresponding to the OPCODE function.
pub fn get_result_type(op: OPCODE) -> (u32, ObjectType) {
    match op {
        OPCODE::AES => (0, ObjectType::NotAnObject), //Not implemented
        OPCODE::SHA256 => (32, ObjectType::Unsigned(8)),
        OPCODE::Blake2s => (32, ObjectType::Unsigned(8)),
        OPCODE::HashToField => (1, ObjectType::NativeField),
        OPCODE::MerkleMembership => (1, ObjectType::NativeField), //or bool?
        OPCODE::SchnorrVerify => (1, ObjectType::NativeField),    //or bool?
        OPCODE::Pedersen => (2, ObjectType::NativeField),
        OPCODE::EcdsaSecp256k1 => (1, ObjectType::NativeField), //field?
        OPCODE::FixedBaseScalarMul => (2, ObjectType::NativeField),
        OPCODE::ToBits => (FieldElement::max_num_bits(), ObjectType::Boolean),
    }
}

//Lowlevel functions with no more than 2 arguments
pub fn call_low_level(
    op: OPCODE,
    call_expr: HirCallExpression,
    igen: &mut IRGenerator,
    env: &mut Environment,
) -> Result<NodeId, RuntimeError> {
    //Inputs
    let mut args: Vec<NodeId> = Vec::new();

    for arg in &call_expr.arguments {
        if let Ok(lhs) = igen.codegen_expression(env, arg) {
            args.push(lhs.unwrap_id()); //TODO handle multiple values
        } else {
            panic!("error calling {}", op);
        }
    }
    //REM: we do not check that the nb of inputs correspond to the function signature, it is done in the frontend

    //Output:
    let result_signature = get_result_type(op);
    let result_type = if result_signature.0 > 1 {
        //We create an array that will contain the result and set the res_type to point to that array
        let result_index = igen.context.mem.create_new_array(
            result_signature.0,
            result_signature.1,
            &format!("{}_result", op),
        );
        node::ObjectType::Pointer(result_index)
    } else {
        result_signature.1
    };

    //when the function returns an array, we use ins.res_type(array)
    //else we map ins.id to the returned witness
    //Call instruction
    igen.context.new_instruction(node::Operation::Intrinsic(op, args), result_type)
}

pub fn param_to_ident(patern: &HirPattern, mutable: bool) -> Vec<(&HirIdent, bool)> {
    match &patern {
        HirPattern::Identifier(id) => vec![(id, mutable)],
        HirPattern::Mutable(pattern, _) => param_to_ident(pattern.as_ref(), true),
        HirPattern::Tuple(v, _) => {
            let mut result = Vec::new();
            for pattern in v {
                result.extend(param_to_ident(pattern, mutable));
            }
            result
        }
        HirPattern::Struct(_, v, _) => {
            let mut result = Vec::new();
            for (_, pattern) in v {
                result.extend(param_to_ident(pattern, mutable));
            }
            result
        }
    }
}

pub fn call_builtin(ctx: &mut SsaContext, builtin_name: &str, arguments: Vec<NodeId>) -> NodeId {
    //we only have one for now
    assert_eq!(builtin_name, "array_len");
    if let Some(a) = super::mem::Memory::deref(ctx, arguments[0]) {
        let x = ctx.mem.len(a);
        return ctx.get_or_create_const(FieldElement::from(x as i128), ObjectType::NativeField);
    }
    unreachable!("invalid argument for array_len()");
}

#[allow(clippy::too_many_arguments)]
pub fn create_function(
    igen: &mut IRGenerator,
    func_id: FuncId,
    name: &str,
    context: &noirc_frontend::hir::Context,
    env: &mut Environment,
    parameters: &Parameters,
    arguments: &HashMap<usize, u32>,
    index: FuncIndex,
) -> Result<(), RuntimeError> {
    let current_block = igen.context.current_block;
    let current_function = igen.function_context;
    let func_block = super::block::BasicBlock::create_cfg(&mut igen.context);

    let mut func = SSAFunction::new(func_id, name, func_block, index, &igen.context);

    let function = context.def_interner.function(&func_id);
    let block = function.block(&context.def_interner);
    //argumemts:
    let mut pos = 0;
    let mut template_args = Vec::new();
    for pat in parameters.iter() {
        //For now we use the mut property of the argument to indicate if it is modified or not
        //TODO: check instead in the function body whether there is a store for the array
        let ident_ids = param_to_ident(&pat.0, false);
        for def in ident_ids.iter().enumerate() {
            let node_ids = ssa_form::create_function_parameter(
                igen,
                &def.1 .0.id,
                arguments,
                &mut pos,
                &mut template_args,
            );
            let e: Vec<(NodeId, bool)> = node_ids.iter().map(|n| (*n, def.1 .1)).collect();
            func.arguments.extend(e);
        }
    }
    for i in template_args {
        let array_id = super::mem::Memory::deref(&igen.context, func.arguments[i.0].0).unwrap();
        func.template.push(i.0, array_id, i.1);
    }
    //push into the 'call stack'
    igen.context.mem.push_template(func.template.template_arguments());
    igen.function_context = Some(index);

    let myid = igen.context.insert_ssafunc(func);
    let last_value = igen.codegen_block(block.statements(), env);
    let returned_values = last_value.to_node_ids();
    let mut func = igen.context.get_ssafunc(myid).unwrap().clone();
    for i in &returned_values {
        if let Some(node) = igen.context.try_get_node(*i) {
            func.result_types.push(node.get_type());
        } else {
            func.result_types.push(ObjectType::NotAnObject);
        }
    }
    igen.context
        .new_instruction(node::Operation::Return(returned_values), node::ObjectType::NotAnObject)?;
    let decision = func.compile(igen)?; //unroll the function
    func.decision = decision;

    igen.context.update_ssafunc(func);
    //pop the 'call stack'
    igen.context.current_block = current_block;
    igen.function_context = current_function;
    igen.context.mem.pop_template();
    Ok(())
}

pub fn resize_graph(call_graph: &mut Vec<Vec<u8>>, size: usize) {
    while call_graph.len() < size {
        call_graph.push(vec![0; size]);
    }

    for i in call_graph.iter_mut() {
        while i.len() < size {
            i.push(0);
        }
    }
}

pub fn update_call_graph(call_graph: &mut Vec<Vec<u8>>, caller: FuncIndex, callee: FuncIndex) {
    let a = caller.0;
    let b = callee.0;
    let max = a.max(b) + 1;
    resize_graph(call_graph, max);

    call_graph[a][b] = 1;
}

fn is_leaf(call_graph: &[Vec<u8>], i: FuncIndex) -> bool {
    for j in 0..call_graph[i.0].len() {
        if call_graph[i.0][j] == 1 {
            return false;
        }
    }
    true
}

fn get_new_leaf(ctx: &SsaContext, processed: &[FuncIndex]) -> (FuncIndex, SsaFuncId) {
    for f_vec in ctx.functions.values() {
        for f in f_vec {
            if !processed.contains(&(f.idx)) && is_leaf(&ctx.call_graph, f.idx) {
                return (f.idx, f.id);
            }
        }
    }
    unimplemented!("Recursive function call is not supported");
}

//inline all functions of the call graph such that every inlining operates with a fully flattened function
pub fn inline_all(ctx: &mut SsaContext) -> Result<(), RuntimeError> {
    let len = ctx.get_function_nb();
    resize_graph(&mut ctx.call_graph, len);
    let l = ctx.call_graph.len();
    let mut processed = Vec::new();
    while processed.len() < l {
        let i = get_new_leaf(ctx, &processed);
        if !processed.is_empty() {
            super::optim::full_cse(ctx, ctx.get_ssafunc(i.1).unwrap().entry_block)?;
        }
        let mut to_inline = Vec::new();
        for f_vec in ctx.functions.values() {
            for f in f_vec {
                if ctx.call_graph[f.idx.0][i.0 .0] == 1 {
                    to_inline.push((f.id, f.idx));
                }
            }
        }
        for (func_id, func_idx) in to_inline {
            super::inline::inline_cfg(ctx, func_id, Some(i.1))?;
            ctx.call_graph[func_idx.0][i.0 .0] = 0;
        }
        processed.push(i.0);
    }
    ctx.call_graph.clear();
    Ok(())
}
