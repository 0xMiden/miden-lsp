use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use miden_assembly_syntax::{
    Felt, Parse, ParseOptions,
    ast::{
        Block, FunctionType as AstFunctionType, Instruction, InvocationTarget, ModuleKind, Op,
        TypeExpr, types,
    },
    debuginfo::{DefaultSourceManager, SourceSpan, Spanned},
};
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, InlayHint, InlayHintKind, InlayHintLabel, MarkupContent,
    MarkupKind, Position, Range,
};

use crate::{
    analysis::{ItemKind, ReferenceKind},
    document::byte_range_to_lsp_range,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StackSignature {
    pub args: usize,
    pub results: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct StackCallableDefinition {
    pub path: String,
    pub kind: ItemKind,
    pub signature: Option<StackSignature>,
}

#[derive(Clone, Debug)]
pub(crate) struct StackResolvedReference {
    pub range: Range,
    pub kind: ReferenceKind,
    pub definition_indexes: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct StackModuleInput {
    pub file_path: PathBuf,
    pub module_path: String,
    pub text: String,
    pub line_offsets: Vec<usize>,
    pub executable_root: bool,
    pub resolved_references: Vec<StackResolvedReference>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StackDocumentAnalysis {
    diagnostics: Vec<Diagnostic>,
    overlays: Vec<StackOverlay>,
}

impl StackDocumentAnalysis {
    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub(crate) fn hover_markdown_at(&self, position: Position) -> Option<(Range, String)> {
        self.overlays
            .iter()
            .find(|overlay| contains_position(overlay.range, position))
            .map(|overlay| (overlay.range, overlay.hover_markdown.clone()))
    }

    pub(crate) fn inlay_hints(&self, visible_range: Range) -> Vec<InlayHint> {
        self.overlays
            .iter()
            .filter(|overlay| overlay.show_inlay && ranges_overlap(overlay.range, visible_range))
            .map(|overlay| InlayHint {
                position: overlay.range.end,
                label: InlayHintLabel::String(format!(" {}", overlay.inlay_label)),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: Some(
                    MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: overlay.hover_markdown.clone(),
                    }
                    .into(),
                ),
                padding_left: Some(true),
                padding_right: None,
                data: None,
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
struct StackOverlay {
    range: Range,
    hover_markdown: String,
    inlay_label: String,
    show_inlay: bool,
}

#[derive(Clone, Debug)]
struct DocumentContext {
    file_path: PathBuf,
    module_path: String,
    text: Arc<String>,
    line_offsets: Arc<Vec<usize>>,
    resolved_references: BTreeMap<RangeKey, StackResolvedReference>,
}

#[derive(Clone, Debug)]
struct SourceProcedure {
    path: String,
    explicit_signature: Option<StackSignature>,
    body: Block,
    document: Arc<DocumentContext>,
}

#[derive(Clone, Debug)]
enum CallEffect {
    Counts(StackSignature),
    Concrete(ConcreteEffect),
    Indeterminate,
}

#[derive(Clone, Debug)]
struct ConcreteEffect {
    required_inputs: usize,
    results: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Input(usize),
    Unknown,
    Felt(Felt),
    Address(u32),
    ProcRef { path: String, index: u8 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    stack: VecDeque<Value>,
    next_input: usize,
    highest_input_touched: usize,
    memory: BTreeMap<u32, Value>,
}

impl State {
    fn ensure_depth(&mut self, depth: usize) {
        while self.stack.len() < depth {
            self.stack.push_back(Value::Input(self.next_input));
            self.next_input += 1;
        }
    }

    fn touch(&mut self, value: &Value) {
        if let Value::Input(index) = value {
            self.highest_input_touched = self.highest_input_touched.max(index + 1);
        }
    }

    fn pop(&mut self) -> Value {
        self.ensure_depth(1);
        let value = self.stack.pop_front().expect("stack should contain at least one value");
        self.touch(&value);
        value
    }

    fn pop_many(&mut self, count: usize) -> Vec<Value> {
        (0..count).map(|_| self.pop()).collect()
    }

    fn peek_many(&mut self, count: usize) -> Vec<Value> {
        self.ensure_depth(count);
        let values = self.stack.iter().take(count).cloned().collect::<Vec<_>>();
        for value in &values {
            self.touch(value);
        }
        values
    }

    fn push(&mut self, value: Value) {
        self.stack.push_front(value);
    }

    fn push_many<I>(&mut self, values: I)
    where
        I: IntoIterator<Item = Value>,
    {
        let mut values = values.into_iter().collect::<Vec<_>>();
        while let Some(value) = values.pop() {
            self.push(value);
        }
    }

    fn set_top_many<I>(&mut self, count: usize, values: I)
    where
        I: IntoIterator<Item = Value>,
    {
        self.pop_many(count);
        self.push_many(values);
    }

    fn replace_top_many<I>(&mut self, count: usize, values: I)
    where
        I: IntoIterator<Item = Value>,
    {
        let removed = self.stack.len().min(count);
        for _ in 0..removed {
            self.stack.pop_front();
        }
        self.push_many(values);
    }

    fn clear_known_memory(&mut self) {
        self.memory.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RangeKey {
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

impl From<Range> for RangeKey {
    fn from(range: Range) -> Self {
        Self {
            start_line: range.start.line,
            start_character: range.start.character,
            end_line: range.end.line,
            end_character: range.end.character,
        }
    }
}

struct Analyzer {
    callables_by_index: BTreeMap<usize, StackCallableDefinition>,
    callables_by_path: BTreeMap<String, StackCallableDefinition>,
    procedures: BTreeMap<String, SourceProcedure>,
    procedure_effects: BTreeMap<String, CallEffect>,
    in_progress: BTreeSet<String>,
    documents: BTreeMap<PathBuf, StackDocumentAnalysis>,
}

pub(crate) fn analyze_modules(
    inputs: &[StackModuleInput],
    callables_by_index: BTreeMap<usize, StackCallableDefinition>,
) -> BTreeMap<PathBuf, StackDocumentAnalysis> {
    let mut procedures = BTreeMap::new();
    let mut documents = BTreeMap::new();

    for input in inputs {
        documents.entry(input.file_path.clone()).or_default();
        let Some(document) = parse_module_input(input) else {
            continue;
        };

        for procedure in document {
            procedures.insert(procedure.path.clone(), procedure);
        }
    }

    let callables_by_path = callables_by_index
        .values()
        .cloned()
        .map(|definition| (definition.path.clone(), definition))
        .collect();

    let mut analyzer = Analyzer {
        callables_by_index,
        callables_by_path,
        procedures,
        procedure_effects: BTreeMap::new(),
        in_progress: BTreeSet::new(),
        documents,
    };

    let paths = analyzer.procedures.keys().cloned().collect::<Vec<_>>();
    for path in paths {
        let _ = analyzer.ensure_body_effect(&path);
    }

    analyzer.documents
}

pub(crate) fn signature_from_function_type(
    signature: &types::FunctionType,
) -> Option<StackSignature> {
    let args = signature
        .params()
        .iter()
        .map(|ty| Some(ty.size_in_felts()))
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .sum();
    let results = signature
        .results()
        .iter()
        .map(|ty| Some(ty.size_in_felts()))
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .sum();

    Some(StackSignature { args, results })
}

fn parse_module_input(input: &StackModuleInput) -> Option<Vec<SourceProcedure>> {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let kind = if input.executable_root {
        ModuleKind::Executable
    } else {
        ModuleKind::Library
    };
    let module = input
        .text
        .clone()
        .parse_with_options(source_manager, ParseOptions::new(kind, input.module_path.as_str()))
        .ok()?;

    let document = Arc::new(DocumentContext {
        file_path: input.file_path.clone(),
        module_path: input.module_path.clone(),
        text: Arc::new(input.text.clone()),
        line_offsets: Arc::new(input.line_offsets.clone()),
        resolved_references: input
            .resolved_references
            .iter()
            .cloned()
            .map(|reference| (RangeKey::from(reference.range), reference))
            .collect(),
    });

    let local_types = module
        .types()
        .map(|decl| (decl.name().as_str().to_string(), decl.ty()))
        .collect::<BTreeMap<_, _>>();

    Some(
        module
            .procedures()
            .map(|procedure| SourceProcedure {
                path: format!("{}::{}", input.module_path, procedure.name()),
                explicit_signature: procedure
                    .signature()
                    .and_then(|signature| source_signature(signature, &local_types)),
                body: procedure.body().clone(),
                document: Arc::clone(&document),
            })
            .collect(),
    )
}

fn source_signature(
    signature: &AstFunctionType,
    local_types: &BTreeMap<String, TypeExpr>,
) -> Option<StackSignature> {
    let args = signature
        .args
        .iter()
        .map(|ty| type_expr_size_in_felts(ty, local_types))
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .sum();
    let results = signature
        .results
        .iter()
        .map(|ty| type_expr_size_in_felts(ty, local_types))
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .sum();

    Some(StackSignature { args, results })
}

fn type_expr_size_in_felts(
    expr: &TypeExpr,
    local_types: &BTreeMap<String, TypeExpr>,
) -> Option<usize> {
    type_expr_to_concrete_type(expr, local_types, 0).map(|ty| ty.size_in_felts())
}

fn type_expr_to_concrete_type(
    expr: &TypeExpr,
    local_types: &BTreeMap<String, TypeExpr>,
    depth: usize,
) -> Option<types::Type> {
    if depth > 64 {
        return None;
    }

    match expr {
        TypeExpr::Primitive(ty) => Some(ty.inner().clone()),
        TypeExpr::Array(array) => Some(types::Type::Array(Arc::new(types::ArrayType::new(
            type_expr_to_concrete_type(&array.elem, local_types, depth + 1)?,
            array.arity,
        )))),
        TypeExpr::Ptr(pointer) => Some(types::Type::Ptr(Arc::new(types::PointerType::new(
            type_expr_to_concrete_type(&pointer.pointee, local_types, depth + 1)?,
        )))),
        TypeExpr::Struct(struct_ty) => {
            let fields = struct_ty
                .fields
                .iter()
                .map(|field| type_expr_to_concrete_type(&field.ty, local_types, depth + 1))
                .collect::<Option<Vec<_>>>()?;
            Some(types::Type::Struct(Arc::new(types::StructType::from_parts(
                struct_ty.name.clone().map(|name| name.into_inner()),
                struct_ty.repr.into_inner(),
                fields,
            ))))
        }
        TypeExpr::Ref(path) => {
            let path = path.inner();
            let local = path
                .as_ident()
                .map(|ident| ident.as_str().to_string())
                .or_else(|| path.last().map(str::to_string))?;
            type_expr_to_concrete_type(local_types.get(local.as_str())?, local_types, depth + 1)
        }
    }
}

impl Analyzer {
    fn ensure_body_effect(&mut self, path: &str) -> CallEffect {
        if let Some(effect) = self.procedure_effects.get(path) {
            return effect.clone();
        }
        if !self.in_progress.insert(path.to_string()) {
            return CallEffect::Indeterminate;
        }

        let procedure = match self.procedures.get(path) {
            Some(procedure) => procedure.clone(),
            None => {
                self.in_progress.remove(path);
                return CallEffect::Indeterminate;
            }
        };

        let effect = self.analyze_body(&procedure);
        self.in_progress.remove(path);
        self.procedure_effects.insert(path.to_string(), effect.clone());
        effect
    }

    fn effective_call_effect(&mut self, path: &str) -> CallEffect {
        if let Some(procedure) = self.procedures.get(path)
            && let Some(signature) = procedure.explicit_signature.clone()
        {
            return CallEffect::Counts(signature);
        }

        if self.procedures.contains_key(path) {
            return self.ensure_body_effect(path);
        }

        self.callables_by_path
            .get(path)
            .and_then(|callable| callable.signature.clone())
            .map(CallEffect::Counts)
            .unwrap_or(CallEffect::Indeterminate)
    }

    fn analyze_body(&mut self, procedure: &SourceProcedure) -> CallEffect {
        let state = match self.analyze_block(procedure, &procedure.body, State::default()) {
            Some(state) => state,
            None => return CallEffect::Indeterminate,
        };

        let required_inputs = state.highest_input_touched;
        let values = state.stack.into_iter().collect::<Vec<_>>();
        let tail_start = values
            .iter()
            .position(|value| matches!(value, Value::Input(index) if *index == required_inputs))
            .unwrap_or(values.len());

        if values[tail_start..].iter().enumerate().any(|(offset, value)| {
            !matches!(value, Value::Input(index) if *index == required_inputs + offset)
        }) {
            return CallEffect::Indeterminate;
        }

        CallEffect::Concrete(ConcreteEffect {
            required_inputs,
            results: values[..tail_start].to_vec(),
        })
    }

    fn analyze_block(
        &mut self,
        procedure: &SourceProcedure,
        block: &Block,
        mut state: State,
    ) -> Option<State> {
        for op in block.iter() {
            let range = span_to_range(&procedure.document, op.span());
            let result = match op {
                Op::Inst(inst) => self.analyze_instruction(procedure, inst, state),
                Op::If {
                    then_blk, else_blk, ..
                } => self.analyze_if(procedure, range, then_blk, else_blk, state),
                Op::While { body, .. } => self.analyze_while(procedure, range, body, state),
                Op::Repeat { count, body, .. } => {
                    self.analyze_repeat(procedure, range, count.expect_value(), body, state)
                }
            };

            match result {
                StepResult::Continue {
                    state: next,
                    overlay,
                } => {
                    self.push_overlay(
                        &procedure.document.file_path,
                        StackOverlay {
                            range,
                            hover_markdown: overlay.hover_markdown,
                            inlay_label: overlay.inlay_label,
                            show_inlay: overlay.show_inlay,
                        },
                    );
                    state = next;
                }
                StepResult::Indeterminate {
                    diagnostic,
                    overlay,
                } => {
                    if let Some(diagnostic) = diagnostic {
                        self.push_diagnostic(&procedure.document.file_path, diagnostic);
                    }
                    self.push_overlay(
                        &procedure.document.file_path,
                        StackOverlay {
                            range,
                            hover_markdown: overlay.hover_markdown,
                            inlay_label: overlay.inlay_label,
                            show_inlay: overlay.show_inlay,
                        },
                    );
                    return None;
                }
            }
        }

        Some(state)
    }

    fn analyze_if(
        &mut self,
        procedure: &SourceProcedure,
        range: Range,
        then_blk: &Block,
        else_blk: &Block,
        mut state: State,
    ) -> StepResult {
        let condition = state.pop();
        let base = state.clone();

        let known_branch = match condition {
            Value::Felt(value) if value == Felt::ZERO => Some(false),
            Value::Felt(value) if value == Felt::ONE => Some(true),
            _ => None,
        };

        match known_branch {
            Some(true) => match self.analyze_block(procedure, then_blk, base) {
                Some(next) => {
                    StepResult::continue_with_highlight(next, 1, 0, "branch on known `true`")
                }
                None => StepResult::indeterminate_without_diagnostic(
                    "stack effect undetermined after `if.true`",
                ),
            },
            Some(false) => match self.analyze_block(procedure, else_blk, base) {
                Some(next) => {
                    StepResult::continue_with_highlight(next, 1, 0, "branch on known `false`")
                }
                None => StepResult::indeterminate_without_diagnostic(
                    "stack effect undetermined after `if.true`",
                ),
            },
            None => {
                let then_state = self.analyze_block(procedure, then_blk, base.clone());
                let else_state = self.analyze_block(procedure, else_blk, base);
                match (then_state, else_state) {
                    (Some(then_state), Some(else_state)) if then_state == else_state => {
                        StepResult::continue_with_highlight(
                            then_state,
                            1,
                            0,
                            "balanced conditional",
                        )
                    }
                    _ => StepResult::warning(
                        range,
                        "conditional branches leave different stack states; stack effects are \
                         undetermined after this `if.true`",
                    ),
                }
            }
        }
    }

    fn analyze_while(
        &mut self,
        procedure: &SourceProcedure,
        range: Range,
        body: &Block,
        mut state: State,
    ) -> StepResult {
        let condition = state.pop();
        let skip_state = state.clone();

        if matches!(condition, Value::Felt(value) if value == Felt::ZERO) {
            return StepResult::continue_with_highlight(
                skip_state,
                1,
                0,
                "loop skipped on known `false`",
            );
        }

        let Some(body_state) = self.analyze_block(procedure, body, skip_state.clone()) else {
            return StepResult::indeterminate_without_diagnostic(
                "stack effect undetermined after `while.true`",
            );
        };
        let mut exit_state = body_state.clone();
        let _ = exit_state.pop();

        if exit_state == skip_state {
            StepResult::continue_with_highlight(skip_state, 1, 0, "balanced loop")
        } else {
            StepResult::warning(
                range,
                "loop body does not preserve the carried stack state; stack effects are \
                 undetermined after this `while.true`",
            )
        }
    }

    fn analyze_repeat(
        &mut self,
        procedure: &SourceProcedure,
        _range: Range,
        count: u32,
        body: &Block,
        mut state: State,
    ) -> StepResult {
        for _ in 0..count {
            let Some(next) = self.analyze_block(procedure, body, state) else {
                return StepResult::indeterminate_without_diagnostic(
                    "stack effect undetermined after `repeat`",
                );
            };
            state = next;
        }

        StepResult::continue_with(state, 0, 0, &format!("repeat.{count}"))
    }

    fn analyze_instruction(
        &mut self,
        procedure: &SourceProcedure,
        inst: &miden_assembly_syntax::debuginfo::Span<Instruction>,
        mut state: State,
    ) -> StepResult {
        use Instruction::*;

        match inst.inner() {
            Nop | Debug(_) | DebugVar(_) | Trace(_) | SysEvent(_) | Emit => {
                StepResult::continue_with(state, 0, 0, &format!("`{}`", inst.inner()))
            }
            EmitImm(_) => StepResult::continue_with(state, 0, 0, "immediate `emit`"),
            Push(immediate) => {
                let value = immediate.expect_value();
                match value {
                    miden_assembly_syntax::parser::PushValue::Int(value) => {
                        state.push(Value::Felt(match value {
                            miden_assembly_syntax::parser::IntValue::U8(value) => Felt::from(value),
                            miden_assembly_syntax::parser::IntValue::U16(value) => {
                                Felt::from(value)
                            }
                            miden_assembly_syntax::parser::IntValue::U32(value) => {
                                Felt::from(value)
                            }
                            miden_assembly_syntax::parser::IntValue::Felt(value) => value,
                        }))
                    }
                    miden_assembly_syntax::parser::PushValue::Word(word) => {
                        state.push_many(word.0.into_iter().map(Value::Felt))
                    }
                }
                StepResult::continue_with(
                    state,
                    0,
                    stack_depth_delta(inst.inner()),
                    &format!("`{}`", inst.inner()),
                )
            }
            PushSlice(_, range) => {
                let len = range.end.saturating_sub(range.start);
                state.push_many((0..len).map(|_| Value::Unknown));
                StepResult::continue_with(state, 0, len, "word slice push")
            }
            PushFeltList(values) => {
                let len = values.len();
                state.push_many(values.iter().copied().map(Value::Felt));
                StepResult::continue_with(state, 0, len, "felt list push")
            }
            PadW => {
                state.push_many((0..4).map(|_| Value::Felt(Felt::ZERO)));
                StepResult::continue_with(state, 0, 4, "`padw`")
            }
            DropW => {
                let _ = state.pop_many(4);
                StepResult::continue_with(state, 4, 0, "`dropw`")
            }
            Locaddr(offset) => {
                state.push(Value::Address(offset.expect_value() as u32));
                StepResult::continue_with(state, 0, 1, "`locaddr`")
            }
            Sdepth | Clk => {
                state.push(Value::Unknown);
                StepResult::continue_with(state, 0, 1, &format!("`{}`", inst.inner()))
            }
            Caller => {
                state.ensure_depth(4);
                state.set_top_many(4, (0..4).map(|_| Value::Unknown));
                StepResult::continue_with(state, 4, 4, "`caller`")
            }
            AdvPush(count) => {
                let count = count.expect_value() as usize;
                state.push_many((0..count).map(|_| Value::Unknown));
                StepResult::continue_with(state, 0, count, "`adv.push`")
            }
            AdvLoadW => {
                state.ensure_depth(4);
                state.set_top_many(4, (0..4).map(|_| Value::Unknown));
                StepResult::continue_with(state, 4, 4, "`adv.loadw`")
            }
            MemLoad | MemLoadImm(_) | LocLoad(_) => {
                let value = load_scalar(inst.inner(), &mut state);
                state.push(value);
                StepResult::continue_with(
                    state,
                    scalar_load_inputs(inst.inner()),
                    1,
                    &format!("`{}`", inst.inner()),
                )
            }
            MemLoadWBe | MemLoadWBeImm(_) | MemLoadWLe | MemLoadWLeImm(_) | LocLoadWBe(_)
            | LocLoadWLe(_) => {
                let values = load_word(inst.inner(), &mut state);
                state.replace_top_many(4, values);
                StepResult::continue_with(
                    state,
                    word_load_inputs(inst.inner()),
                    4,
                    &format!("`{}`", inst.inner()),
                )
            }
            MemStore | MemStoreImm(_) | LocStore(_) => {
                let popped = store_scalar(inst.inner(), &mut state);
                StepResult::continue_with(state, popped, 0, &format!("`{}`", inst.inner()))
            }
            MemStoreWBe | MemStoreWBeImm(_) | MemStoreWLe | MemStoreWLeImm(_) | LocStoreWBe(_)
            | LocStoreWLe(_) => {
                let popped = store_word(inst.inner(), &mut state);
                StepResult::continue_with(state, popped, 0, &format!("`{}`", inst.inner()))
            }
            ProcRef(target) => {
                let path = self.resolve_target_path(procedure, target);
                let values = if let Some(path) = path {
                    (0..4)
                        .map(|index| Value::ProcRef {
                            path: path.clone(),
                            index,
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![Value::Unknown; 4]
                };
                state.push_many(values);
                StepResult::continue_with(state, 0, 4, "`procref`")
            }
            Exec(target) | Call(target) | SysCall(target) => {
                self.apply_call_effect(procedure, target, &mut state)
            }
            DynExec | DynCall => self.apply_dynamic_call(inst.inner(), &mut state),
            Hash => {
                let _ = state.pop_many(4);
                state.push_many((0..4).map(|_| Value::Unknown));
                StepResult::continue_with(state, 4, 4, "`hash`")
            }
            HMerge => {
                let _ = state.pop_many(8);
                state.push_many((0..4).map(|_| Value::Unknown));
                StepResult::continue_with(state, 8, 4, "`hmerge`")
            }
            HPerm => {
                state.ensure_depth(12);
                state.set_top_many(12, (0..12).map(|_| Value::Unknown));
                StepResult::continue_with(state, 12, 12, "`hperm`")
            }
            MTreeGet => {
                let _ = state.pop_many(6);
                state.push_many((0..8).map(|_| Value::Unknown));
                StepResult::continue_with(state, 6, 8, "`mtree.get`")
            }
            MTreeSet => {
                let _ = state.pop_many(10);
                state.push_many((0..8).map(|_| Value::Unknown));
                StepResult::continue_with(state, 10, 8, "`mtree.set`")
            }
            MTreeMerge => {
                let _ = state.pop_many(8);
                state.push_many((0..4).map(|_| Value::Unknown));
                StepResult::continue_with(state, 8, 4, "`mtree.merge`")
            }
            MTreeVerify | MTreeVerifyWithError(_) => {
                state.ensure_depth(10);
                StepResult::continue_with(state, 10, 10, "`mtree.verify`")
            }
            CryptoStream => {
                state.ensure_depth(14);
                state.set_top_many(14, (0..14).map(|_| Value::Unknown));
                StepResult::continue_with(state, 14, 14, "`cryptostream`")
            }
            MemStream | AdvPipe => {
                state.ensure_depth(13);
                state.set_top_many(13, (0..13).map(|_| Value::Unknown));
                StepResult::continue_with(state, 13, 13, &format!("`{}`", inst.inner()))
            }
            FriExt2Fold4 | HornerBase | HornerExt | EvalCircuit | LogPrecompile => {
                StepResult::indeterminate_without_diagnostic(&format!(
                    "stack effect unsupported for `{}`",
                    inst.inner()
                ))
            }
            _ => self.apply_generic_instruction(inst.inner(), &mut state),
        }
    }

    fn apply_call_effect(
        &mut self,
        procedure: &SourceProcedure,
        target: &InvocationTarget,
        state: &mut State,
    ) -> StepResult {
        let range = span_to_range(&procedure.document, target.span());
        let Some(path) = self.resolve_target_path(procedure, target) else {
            return StepResult::warning(
                range,
                "callee stack effects could not be resolved; stack effects are undetermined after \
                 this call",
            );
        };

        match self.effective_call_effect(&path) {
            CallEffect::Counts(signature) => {
                let _ = state.pop_many(signature.args);
                state.push_many((0..signature.results).map(|_| Value::Unknown));
                StepResult::continue_with(
                    state.clone(),
                    signature.args,
                    signature.results,
                    &format!("resolved call `{path}`"),
                )
            }
            CallEffect::Concrete(effect) => {
                let args = state.pop_many(effect.required_inputs);
                let result_count = effect.results.len();
                state.push_many(
                    effect.results.into_iter().map(|value| substitute_inputs(value, &args)),
                );
                StepResult::continue_with_highlight(
                    state.clone(),
                    args.len(),
                    result_count,
                    &format!("resolved call `{path}` from source body"),
                )
            }
            CallEffect::Indeterminate => StepResult::warning(
                range,
                "callee stack effects are indeterminate; stack effects are undetermined after \
                 this call",
            ),
        }
    }

    fn apply_dynamic_call(&mut self, instruction: &Instruction, state: &mut State) -> StepResult {
        let address = state.pop();
        if let Some(addr) = known_address(&address)
            && let Some(path) = proc_ref_from_memory(&state.memory, addr)
        {
            return match self.effective_call_effect(&path) {
                CallEffect::Counts(signature) => {
                    let _ = state.pop_many(signature.args);
                    state.push_many((0..signature.results).map(|_| Value::Unknown));
                    StepResult::continue_with_highlight(
                        state.clone(),
                        signature.args + 1,
                        signature.results,
                        &format!("resolved `{instruction}` via `procref`"),
                    )
                }
                CallEffect::Concrete(effect) => {
                    let args = state.pop_many(effect.required_inputs);
                    let result_count = effect.results.len();
                    state.push_many(
                        effect.results.into_iter().map(|value| substitute_inputs(value, &args)),
                    );
                    StepResult::continue_with_highlight(
                        state.clone(),
                        args.len() + 1,
                        result_count,
                        &format!("resolved `{instruction}` via `procref`"),
                    )
                }
                CallEffect::Indeterminate => StepResult::indeterminate_without_diagnostic(
                    &format!("stack effect undetermined after `{instruction}`"),
                ),
            };
        }

        StepResult::indeterminate_without_diagnostic(&format!(
            "dynamic callee could not be traced for `{instruction}`"
        ))
    }

    fn apply_generic_instruction(
        &mut self,
        instruction: &Instruction,
        state: &mut State,
    ) -> StepResult {
        match generic_effect(instruction) {
            GenericEffect::Transform {
                popped,
                pushed,
                preserve_inputs,
            } => {
                let inputs = state.pop_many(popped);
                let preserved = preserve_inputs.len();
                let outputs = preserve_inputs
                    .into_iter()
                    .map(|index| inputs.get(index).cloned().unwrap_or(Value::Unknown))
                    .chain((0..pushed.saturating_sub(preserved)).map(|_| Value::Unknown))
                    .collect::<Vec<_>>();
                state.push_many(outputs);
                StepResult::continue_with(
                    state.clone(),
                    popped,
                    pushed,
                    &format!("`{instruction}`"),
                )
            }
            GenericEffect::Unsupported => StepResult::indeterminate_without_diagnostic(&format!(
                "stack effect unsupported for `{instruction}`"
            )),
        }
    }

    fn resolve_target_path(
        &self,
        procedure: &SourceProcedure,
        target: &InvocationTarget,
    ) -> Option<String> {
        self.resolve_reference_path(procedure, span_to_range(&procedure.document, target.span()))
            .or_else(|| match target {
                InvocationTarget::MastRoot(_) => None,
                InvocationTarget::Symbol(name) => {
                    Some(format!("{}::{}", procedure.document.module_path, name.as_str()))
                }
                InvocationTarget::Path(path) => Some(path.inner().as_str().to_string()),
            })
    }

    fn resolve_reference_path(&self, procedure: &SourceProcedure, range: Range) -> Option<String> {
        let reference = procedure.document.resolved_references.get(&RangeKey::from(range))?;
        if !matches!(reference.kind, ReferenceKind::Invoke) {
            return None;
        }
        if reference.definition_indexes.len() != 1 {
            return None;
        }
        self.callables_by_index
            .get(&reference.definition_indexes[0])
            .filter(|definition| matches!(definition.kind, ItemKind::Procedure))
            .map(|definition| definition.path.clone())
    }

    fn push_diagnostic(&mut self, file_path: &Path, diagnostic: Diagnostic) {
        self.documents
            .entry(file_path.to_path_buf())
            .or_default()
            .diagnostics
            .push(diagnostic);
    }

    fn push_overlay(&mut self, file_path: &Path, overlay: StackOverlay) {
        self.documents
            .entry(file_path.to_path_buf())
            .or_default()
            .overlays
            .push(overlay);
    }
}

fn substitute_inputs(value: Value, inputs: &[Value]) -> Value {
    match value {
        Value::Input(index) => inputs.get(index).cloned().unwrap_or(Value::Unknown),
        other => other,
    }
}

fn proc_ref_from_memory(memory: &BTreeMap<u32, Value>, address: u32) -> Option<String> {
    let mut path = None;
    for offset in 0..4u32 {
        let value = memory.get(&(address + offset))?;
        match value {
            Value::ProcRef {
                path: candidate,
                index,
            } if *index as u32 == offset => {
                if let Some(path) = &path {
                    if path != candidate {
                        return None;
                    }
                } else {
                    path = Some(candidate.clone());
                }
            }
            _ => return None,
        }
    }

    path
}

fn known_address(value: &Value) -> Option<u32> {
    match value {
        Value::Address(address) => Some(*address),
        Value::Felt(value) => {
            let value = value.as_canonical_u64();
            (value <= u32::MAX as u64).then_some(value as u32)
        }
        Value::Input(_) | Value::Unknown | Value::ProcRef { .. } => None,
    }
}

fn load_scalar(instruction: &Instruction, state: &mut State) -> Value {
    match instruction {
        Instruction::MemLoadImm(address) => {
            state.memory.get(&address.expect_value()).cloned().unwrap_or(Value::Unknown)
        }
        Instruction::LocLoad(offset) => state
            .memory
            .get(&(offset.expect_value() as u32))
            .cloned()
            .unwrap_or(Value::Unknown),
        _ => {
            let address = state.pop();
            known_address(&address)
                .and_then(|address| state.memory.get(&address).cloned())
                .unwrap_or(Value::Unknown)
        }
    }
}

fn load_word(instruction: &Instruction, state: &mut State) -> Vec<Value> {
    let base = match instruction {
        Instruction::MemLoadWBeImm(address) | Instruction::MemLoadWLeImm(address) => {
            Some(address.expect_value())
        }
        Instruction::LocLoadWBe(offset) | Instruction::LocLoadWLe(offset) => {
            Some(offset.expect_value() as u32)
        }
        _ => {
            let address = state.pop();
            known_address(&address)
        }
    };

    let mut word = if let Some(base) = base {
        (0..4u32)
            .map(|offset| state.memory.get(&(base + offset)).cloned().unwrap_or(Value::Unknown))
            .collect::<Vec<_>>()
    } else {
        vec![Value::Unknown; 4]
    };

    if matches!(
        instruction,
        Instruction::MemLoadWBe | Instruction::MemLoadWBeImm(_) | Instruction::LocLoadWBe(_)
    ) {
        word.reverse();
    }

    word
}

fn store_scalar(instruction: &Instruction, state: &mut State) -> usize {
    let value = state.pop();
    let address = match instruction {
        Instruction::MemStoreImm(address) => Some(address.expect_value()),
        Instruction::LocStore(offset) => Some(offset.expect_value() as u32),
        _ => {
            let address = state.pop();
            known_address(&address)
        }
    };

    if let Some(address) = address {
        state.memory.insert(address, value);
    } else {
        state.clear_known_memory();
    }

    scalar_store_inputs(instruction)
}

fn store_word(instruction: &Instruction, state: &mut State) -> usize {
    let address = match instruction {
        Instruction::MemStoreWBeImm(address) | Instruction::MemStoreWLeImm(address) => {
            Some(address.expect_value())
        }
        Instruction::LocStoreWBe(offset) | Instruction::LocStoreWLe(offset) => {
            Some(offset.expect_value() as u32)
        }
        _ => {
            let address = state.pop();
            known_address(&address)
        }
    };

    let mut values = state.peek_many(4);
    if matches!(
        instruction,
        Instruction::MemStoreWBe | Instruction::MemStoreWBeImm(_) | Instruction::LocStoreWBe(_)
    ) {
        values.reverse();
    }

    if let Some(address) = address {
        for (offset, value) in values.into_iter().enumerate() {
            state.memory.insert(address + offset as u32, value);
        }
    } else {
        state.clear_known_memory();
    }

    word_store_inputs(instruction)
}

fn scalar_load_inputs(instruction: &Instruction) -> usize {
    match instruction {
        Instruction::MemLoad => 1,
        Instruction::MemLoadImm(_) | Instruction::LocLoad(_) => 0,
        _ => 0,
    }
}

fn word_load_inputs(instruction: &Instruction) -> usize {
    match instruction {
        Instruction::MemLoadWBe | Instruction::MemLoadWLe => 1,
        Instruction::MemLoadWBeImm(_)
        | Instruction::MemLoadWLeImm(_)
        | Instruction::LocLoadWBe(_)
        | Instruction::LocLoadWLe(_) => 0,
        _ => 0,
    }
}

fn scalar_store_inputs(instruction: &Instruction) -> usize {
    match instruction {
        Instruction::MemStore => 2,
        Instruction::MemStoreImm(_) | Instruction::LocStore(_) => 1,
        _ => 0,
    }
}

fn word_store_inputs(instruction: &Instruction) -> usize {
    match instruction {
        Instruction::MemStoreWBe | Instruction::MemStoreWLe => 1,
        Instruction::MemStoreWBeImm(_)
        | Instruction::MemStoreWLeImm(_)
        | Instruction::LocStoreWBe(_)
        | Instruction::LocStoreWLe(_) => 0,
        _ => 0,
    }
}

#[derive(Clone, Debug)]
enum StepResult {
    Continue {
        state: State,
        overlay: OverlayInfo,
    },
    Indeterminate {
        diagnostic: Option<Diagnostic>,
        overlay: OverlayInfo,
    },
}

impl StepResult {
    fn continue_with(state: State, popped: usize, pushed: usize, detail: &str) -> Self {
        Self::Continue {
            state,
            overlay: OverlayInfo::effect(popped, pushed, detail),
        }
    }

    fn continue_with_highlight(state: State, popped: usize, pushed: usize, detail: &str) -> Self {
        Self::Continue {
            state,
            overlay: OverlayInfo::highlighted_effect(popped, pushed, detail),
        }
    }

    fn warning(range: Range, message: &str) -> Self {
        Self::Indeterminate {
            diagnostic: Some(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                message: message.to_string(),
                ..Diagnostic::default()
            }),
            overlay: OverlayInfo::undetermined(message),
        }
    }

    fn indeterminate_without_diagnostic(message: &str) -> Self {
        Self::Indeterminate {
            diagnostic: None,
            overlay: OverlayInfo::undetermined(message),
        }
    }
}

#[derive(Clone, Debug)]
struct OverlayInfo {
    hover_markdown: String,
    inlay_label: String,
    show_inlay: bool,
}

impl OverlayInfo {
    fn effect(popped: usize, pushed: usize, detail: &str) -> Self {
        let felt_in = if popped == 1 { "felt" } else { "felts" };
        let felt_out = if pushed == 1 { "felt" } else { "felts" };
        Self {
            hover_markdown: format!(
                "**Stack Effect**\n\nConsumes `{popped}` {felt_in} and produces `{pushed}` \
                 {felt_out}.\n\n{detail}"
            ),
            inlay_label: format!("[{popped}->{pushed}]"),
            show_inlay: false,
        }
    }

    fn highlighted_effect(popped: usize, pushed: usize, detail: &str) -> Self {
        let mut overlay = Self::effect(popped, pushed, detail);
        overlay.show_inlay = true;
        overlay
    }

    fn undetermined(message: &str) -> Self {
        Self {
            hover_markdown: format!("**Stack Effect**\n\n{message}"),
            inlay_label: "[?]".to_string(),
            show_inlay: true,
        }
    }
}

#[derive(Clone, Debug)]
enum GenericEffect {
    Transform {
        popped: usize,
        pushed: usize,
        preserve_inputs: Vec<usize>,
    },
    Unsupported,
}

fn generic_effect(instruction: &Instruction) -> GenericEffect {
    use GenericEffect::{Transform, Unsupported};
    use Instruction::*;

    match instruction {
        Assert | AssertWithError(_) | Assertz | AssertzWithError(_) => Transform {
            popped: 1,
            pushed: 0,
            preserve_inputs: vec![],
        },
        AssertEq | AssertEqWithError(_) => Transform {
            popped: 2,
            pushed: 0,
            preserve_inputs: vec![],
        },
        AssertEqw | AssertEqwWithError(_) => Transform {
            popped: 8,
            pushed: 0,
            preserve_inputs: vec![],
        },
        Add | Sub | Mul | Div | And | Or | Xor | Eq | Neq | Lt | Lte | Gt | Gte
        | U32WrappingAdd | U32WrappingSub | U32WrappingMul | U32And | U32Or | U32Xor | U32Lt
        | U32Lte | U32Gt | U32Gte | U32Min | U32Max => Transform {
            popped: 2,
            pushed: 1,
            preserve_inputs: vec![],
        },
        AddImm(_)
        | SubImm(_)
        | MulImm(_)
        | DivImm(_)
        | Neg
        | ILog2
        | Inv
        | Incr
        | Pow2
        | ExpImm(_)
        | ExpBitLength(_)
        | Not
        | EqImm(_)
        | NeqImm(_)
        | IsOdd
        | U32Test
        | U32Assert
        | U32AssertWithError(_)
        | U32Cast
        | U32Not
        | U32ShrImm(_)
        | U32ShlImm(_)
        | U32RotrImm(_)
        | U32RotlImm(_)
        | U32Popcnt
        | U32Ctz
        | U32Clz
        | U32Clo
        | U32Cto
        | EmitImm(_) => Transform {
            popped: 1,
            pushed: 1,
            preserve_inputs: vec![],
        },
        Exp => Transform {
            popped: 2,
            pushed: 1,
            preserve_inputs: vec![],
        },
        Eqw | U32TestW | U32AssertW | U32AssertWWithError(_) => Transform {
            popped: 4,
            pushed: 1,
            preserve_inputs: vec![],
        },
        U32Assert2 | U32Assert2WithError(_) => Transform {
            popped: 2,
            pushed: 2,
            preserve_inputs: vec![0, 1],
        },
        U32Split | U32OverflowingAdd | U32OverflowingSub | U32WideningAdd | U32WideningMul
        | U32Div | U32Mod | U32DivMod => Transform {
            popped: 2,
            pushed: 2,
            preserve_inputs: vec![],
        },
        U32OverflowingAddImm(_)
        | U32OverflowingSubImm(_)
        | U32WideningAddImm(_)
        | U32WideningMulImm(_)
        | U32DivImm(_)
        | U32ModImm(_)
        | U32DivModImm(_) => Transform {
            popped: 1,
            pushed: 2,
            preserve_inputs: vec![],
        },
        U32WrappingAddImm(_) | U32WrappingSubImm(_) | U32WrappingMulImm(_) => Transform {
            popped: 1,
            pushed: 1,
            preserve_inputs: vec![],
        },
        U32OverflowingAdd3 | U32WideningAdd3 => Transform {
            popped: 3,
            pushed: 2,
            preserve_inputs: vec![],
        },
        U32WrappingAdd3 | U32WrappingMadd | U32WideningMadd => Transform {
            popped: 3,
            pushed: 1,
            preserve_inputs: vec![],
        },
        Ext2Add | Ext2Sub | Ext2Div => Transform {
            popped: 4,
            pushed: 2,
            preserve_inputs: vec![],
        },
        Ext2Mul => Transform {
            popped: 4,
            pushed: 2,
            preserve_inputs: vec![],
        },
        Ext2Neg | Ext2Inv => Transform {
            popped: 2,
            pushed: 2,
            preserve_inputs: vec![],
        },
        Drop => Transform {
            popped: 1,
            pushed: 0,
            preserve_inputs: vec![],
        },
        Dup0 => Transform {
            popped: 0,
            pushed: 2,
            preserve_inputs: vec![0, 0],
        },
        Dup1 => Transform {
            popped: 0,
            pushed: 3,
            preserve_inputs: vec![1, 0, 1],
        },
        Dup2 => Transform {
            popped: 0,
            pushed: 4,
            preserve_inputs: vec![2, 0, 1, 2],
        },
        Dup3 => Transform {
            popped: 0,
            pushed: 5,
            preserve_inputs: vec![3, 0, 1, 2, 3],
        },
        Dup4 => Transform {
            popped: 0,
            pushed: 6,
            preserve_inputs: vec![4, 0, 1, 2, 3, 4],
        },
        Dup5 => Transform {
            popped: 0,
            pushed: 7,
            preserve_inputs: vec![5, 0, 1, 2, 3, 4, 5],
        },
        Dup6 => Transform {
            popped: 0,
            pushed: 8,
            preserve_inputs: vec![6, 0, 1, 2, 3, 4, 5, 6],
        },
        Dup7 => Transform {
            popped: 0,
            pushed: 9,
            preserve_inputs: vec![7, 0, 1, 2, 3, 4, 5, 6, 7],
        },
        Dup8 => Transform {
            popped: 0,
            pushed: 10,
            preserve_inputs: vec![8, 0, 1, 2, 3, 4, 5, 6, 7, 8],
        },
        Dup9 => Transform {
            popped: 0,
            pushed: 10,
            preserve_inputs: vec![9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        },
        Dup10 => Transform {
            popped: 0,
            pushed: 11,
            preserve_inputs: vec![10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        },
        Dup11 => Transform {
            popped: 0,
            pushed: 12,
            preserve_inputs: vec![11, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        },
        Dup12 => Transform {
            popped: 0,
            pushed: 13,
            preserve_inputs: vec![12, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        },
        Dup13 => Transform {
            popped: 0,
            pushed: 14,
            preserve_inputs: vec![13, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        },
        Dup14 => Transform {
            popped: 0,
            pushed: 15,
            preserve_inputs: vec![14, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
        },
        Dup15 => Transform {
            popped: 0,
            pushed: 16,
            preserve_inputs: vec![15, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        },
        Swap1 => Transform {
            popped: 2,
            pushed: 2,
            preserve_inputs: vec![1, 0],
        },
        Swap2 => Transform {
            popped: 3,
            pushed: 3,
            preserve_inputs: vec![2, 1, 0],
        },
        Swap3 => Transform {
            popped: 4,
            pushed: 4,
            preserve_inputs: vec![3, 1, 2, 0],
        },
        Swap4 => Transform {
            popped: 5,
            pushed: 5,
            preserve_inputs: vec![4, 1, 2, 3, 0],
        },
        MovUp2 => Transform {
            popped: 3,
            pushed: 3,
            preserve_inputs: vec![2, 0, 1],
        },
        MovUp3 => Transform {
            popped: 4,
            pushed: 4,
            preserve_inputs: vec![3, 0, 1, 2],
        },
        MovUp4 => Transform {
            popped: 5,
            pushed: 5,
            preserve_inputs: vec![4, 0, 1, 2, 3],
        },
        MovUp5 => Transform {
            popped: 6,
            pushed: 6,
            preserve_inputs: vec![5, 0, 1, 2, 3, 4],
        },
        MovUp6 => Transform {
            popped: 7,
            pushed: 7,
            preserve_inputs: vec![6, 0, 1, 2, 3, 4, 5],
        },
        MovUp7 => Transform {
            popped: 8,
            pushed: 8,
            preserve_inputs: vec![7, 0, 1, 2, 3, 4, 5, 6],
        },
        MovUp8 => Transform {
            popped: 9,
            pushed: 9,
            preserve_inputs: vec![8, 0, 1, 2, 3, 4, 5, 6, 7],
        },
        MovDn2 => Transform {
            popped: 3,
            pushed: 3,
            preserve_inputs: vec![1, 2, 0],
        },
        MovDn3 => Transform {
            popped: 4,
            pushed: 4,
            preserve_inputs: vec![1, 2, 3, 0],
        },
        MovDn4 => Transform {
            popped: 5,
            pushed: 5,
            preserve_inputs: vec![1, 2, 3, 4, 0],
        },
        MovDn5 => Transform {
            popped: 6,
            pushed: 6,
            preserve_inputs: vec![1, 2, 3, 4, 5, 0],
        },
        MovDn6 => Transform {
            popped: 7,
            pushed: 7,
            preserve_inputs: vec![1, 2, 3, 4, 5, 6, 0],
        },
        MovDn7 => Transform {
            popped: 8,
            pushed: 8,
            preserve_inputs: vec![1, 2, 3, 4, 5, 6, 7, 0],
        },
        MovDn8 => Transform {
            popped: 9,
            pushed: 9,
            preserve_inputs: vec![1, 2, 3, 4, 5, 6, 7, 8, 0],
        },
        Reversew | Reversedw | SwapW1 | SwapW2 | SwapW3 | SwapDw | CSwap | CSwapW | CDrop
        | CDropW | DupW0 | DupW1 | DupW2 | DupW3 | Swap5 | Swap6 | Swap7 | Swap8 | Swap9
        | Swap10 | Swap11 | Swap12 | Swap13 | Swap14 | Swap15 | MovUp9 | MovUp10 | MovUp11
        | MovUp12 | MovUp13 | MovUp14 | MovUp15 | MovUpW2 | MovUpW3 | MovDn9 | MovDn10
        | MovDn11 | MovDn12 | MovDn13 | MovDn14 | MovDn15 | MovDnW2 | MovDnW3 => Unsupported,
        _ => Unsupported,
    }
}

fn stack_depth_delta(instruction: &Instruction) -> usize {
    match instruction {
        Instruction::Push(immediate) => match immediate.expect_value() {
            miden_assembly_syntax::parser::PushValue::Int(_) => 1,
            miden_assembly_syntax::parser::PushValue::Word(_) => 4,
        },
        _ => 0,
    }
}

fn span_to_range(document: &DocumentContext, span: SourceSpan) -> Range {
    byte_range_to_lsp_range(
        document.text.as_str(),
        document.line_offsets.as_slice(),
        span.into_slice_index(),
    )
}

fn contains_position(range: Range, position: Position) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

fn ranges_overlap(left: Range, right: Range) -> bool {
    !(left.end.line < right.start.line
        || right.end.line < left.start.line
        || (left.end.line == right.start.line && left.end.character <= right.start.character)
        || (right.end.line == left.start.line && right.end.character <= left.start.character))
}
