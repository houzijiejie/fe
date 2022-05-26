use fe_mir::{
    analysis::{
        domtree::DFSet, loop_tree::LoopId, post_domtree::PostIDom, ControlFlowGraph, DomTree,
        LoopTree, PostDomTree,
    },
    ir::{inst::BranchInfo, BasicBlockId, FunctionBody, InstId, ValueId},
};
use fxhash::FxHashSet;

#[derive(Debug, Clone)]
pub(super) enum StructuralInst {
    Inst(InstId),
    If {
        cond: ValueId,
        then: Vec<StructuralInst>,
        else_: Vec<StructuralInst>,
    },
    For {
        body: Vec<StructuralInst>,
    },
    Break,
    Continue,
}

pub(super) struct InstSerializer<'a> {
    body: &'a FunctionBody,
    cfg: ControlFlowGraph,
    loop_tree: LoopTree,
    df: DFSet,
    pd_tree: PostDomTree,
    scope: Option<Scope>,
}

impl<'a> InstSerializer<'a> {
    pub(super) fn new(body: &'a FunctionBody) -> Self {
        let cfg = ControlFlowGraph::compute(body);
        let domtree = DomTree::compute(&cfg);
        let df = domtree.compute_df(&cfg);
        let pd_tree = PostDomTree::compute(body);
        let loop_tree = LoopTree::compute(&cfg, &domtree);

        Self {
            body,
            cfg,
            loop_tree,
            df,
            pd_tree,
            scope: None,
        }
    }

    pub(super) fn serialize(&mut self) -> Vec<StructuralInst> {
        self.scope = None;
        let entry = self.cfg.entry();
        let mut order = vec![];
        self.serialize_block(entry, &mut order);
        order
    }

    fn serialize_block(&mut self, block: BasicBlockId, order: &mut Vec<StructuralInst>) {
        match self.loop_tree.loop_of_block(block) {
            Some(lp)
                if block == self.loop_tree.loop_header(lp)
                    && Some(block) != self.scope.as_ref().and_then(Scope::loop_header) =>
            {
                let loop_exit = self.find_loop_exit(lp);
                self.enter_loop_scope(lp, block, loop_exit);
                let mut body = vec![];
                self.serialize_block(block, &mut body);
                self.exit_scope();
                order.push(StructuralInst::For { body });

                match loop_exit {
                    Some(exit)
                        if self
                            .scope
                            .as_ref()
                            .map(|scope| scope.if_merge_block() != Some(exit))
                            .unwrap_or(true) =>
                    {
                        self.serialize_block(exit, order);
                    }
                    _ => {}
                }

                return;
            }
            _ => {}
        };

        for inst in self.body.order.iter_inst(block) {
            if self.body.store.is_terminator(inst) {
                break;
            }
            if !self.body.store.is_nop(inst) {
                order.push(StructuralInst::Inst(inst));
            }
        }

        let terminator = self.body.order.terminator(&self.body.store, block).unwrap();
        match self.analyze_terminator(terminator) {
            TerminatorInfo::If {
                cond,
                then,
                else_,
                merge_block,
            } => self.serialize_if_terminator(cond, *then, *else_, merge_block, order),
            TerminatorInfo::ToMergeBlock => {}
            TerminatorInfo::Continue => order.push(StructuralInst::Continue),
            TerminatorInfo::Break => order.push(StructuralInst::Break),
            TerminatorInfo::FallThrough(next) => self.serialize_block(next, order),
            TerminatorInfo::NormalInst(inst) => order.push(StructuralInst::Inst(inst)),
        }
    }

    fn serialize_if_terminator(
        &mut self,
        cond: ValueId,
        then: TerminatorInfo,
        else_: TerminatorInfo,
        merge_block: Option<BasicBlockId>,
        order: &mut Vec<StructuralInst>,
    ) {
        let mut then_body = vec![];
        let mut else_body = vec![];

        self.enter_if_scope(merge_block);

        let mut serialize_dest =
            |dest_info, body: &mut Vec<StructuralInst>, merge_block| match dest_info {
                TerminatorInfo::Break => body.push(StructuralInst::Break),
                TerminatorInfo::Continue => body.push(StructuralInst::Continue),
                TerminatorInfo::ToMergeBlock => {}
                TerminatorInfo::FallThrough(dest) => {
                    if Some(dest) != merge_block {
                        self.serialize_block(dest, body);
                    }
                }
                _ => unreachable!(),
            };
        serialize_dest(then, &mut then_body, merge_block);
        serialize_dest(else_, &mut else_body, merge_block);
        self.exit_scope();

        order.push(StructuralInst::If {
            cond,
            then: then_body,
            else_: else_body,
        });
        if let Some(merge_block) = merge_block {
            self.serialize_block(merge_block, order);
        }
    }

    fn enter_loop_scope(&mut self, lp: LoopId, header: BasicBlockId, exit: Option<BasicBlockId>) {
        let kind = ScopeKind::Loop { lp, header, exit };
        let current_scope = std::mem::take(&mut self.scope);
        self.scope = Some(Scope {
            kind,
            parent: current_scope.map(Into::into),
        });
    }

    fn enter_if_scope(&mut self, merge_block: Option<BasicBlockId>) {
        let kind = ScopeKind::If { merge_block };
        let current_scope = std::mem::take(&mut self.scope);
        self.scope = Some(Scope {
            kind,
            parent: current_scope.map(Into::into),
        });
    }

    fn exit_scope(&mut self) {
        let current_scope = std::mem::take(&mut self.scope);
        self.scope = current_scope.unwrap().parent.map(|parent| *parent);
    }

    // NOTE: We assume loop has at most one canonical loop exit.
    fn find_loop_exit(&self, lp: LoopId) -> Option<BasicBlockId> {
        let mut exit_candidates = vec![];
        for block_in_loop in self.loop_tree.iter_blocks_post_order(&self.cfg, lp) {
            for &succ in self.cfg.succs(block_in_loop) {
                if !self.loop_tree.is_block_in_loop(succ, lp) {
                    exit_candidates.push(succ);
                }
            }
        }

        if exit_candidates.is_empty() {
            return None;
        }

        if exit_candidates.len() == 1 {
            let candidate = exit_candidates[0];
            let exit = if let Some(mut df) = self.df.frontiers(candidate) {
                debug_assert_eq!(self.df.frontier_num(candidate), 1);
                df.next()
            } else {
                Some(candidate)
            };
            return exit;
        }

        // If a candidate is a dominance frontier of all other nodes, then the candidate
        // is a loop exit.
        for &cand in &exit_candidates {
            if exit_candidates.iter().all(|&block| {
                if block == cand {
                    true
                } else if let Some(mut df) = self.df.frontiers(block) {
                    df.any(|frontier| frontier == cand)
                } else {
                    true
                }
            }) {
                return Some(cand);
            }
        }

        // If all candidates have the same dominance frontier, then the frontier block
        // is the canonicalized loop exit.
        let mut frontier: FxHashSet<_> = self
            .df
            .frontiers(exit_candidates.pop().unwrap())
            .map(std::iter::Iterator::collect)
            .unwrap_or_default();
        for cand in exit_candidates {
            for cand_frontier in self.df.frontiers(cand).unwrap() {
                if !frontier.contains(&cand_frontier) {
                    frontier.remove(&cand_frontier);
                }
            }
        }
        debug_assert!(frontier.len() < 2);
        frontier.iter().next().copied()
    }

    fn analyze_terminator(&self, inst: InstId) -> TerminatorInfo {
        debug_assert!(self.body.store.is_terminator(inst));

        match self.body.store.branch_info(inst) {
            BranchInfo::Jump(dest) => self.analyze_jump(dest),
            BranchInfo::Branch(cond, then, else_) => {
                self.analyze_branch(self.body.order.inst_block(inst), cond, then, else_)
            }
            BranchInfo::NotBranch => TerminatorInfo::NormalInst(inst),
        }
    }

    fn analyze_branch(
        &self,
        block: BasicBlockId,
        cond: ValueId,
        then: BasicBlockId,
        else_: BasicBlockId,
    ) -> TerminatorInfo {
        let then = Box::new(self.analyze_dest(then));
        let else_ = Box::new(self.analyze_dest(else_));

        let merge_block = match self.pd_tree.post_idom(block) {
            PostIDom::Block(block) => {
                if let Some(lp) = self.scope.as_ref().and_then(Scope::loop_recursive) {
                    if self.loop_tree.is_block_in_loop(block, lp) {
                        Some(block)
                    } else {
                        None
                    }
                } else {
                    Some(block)
                }
            }
            _ => None,
        };

        TerminatorInfo::If {
            cond,
            then,
            else_,
            merge_block,
        }
    }

    fn analyze_jump(&self, dest: BasicBlockId) -> TerminatorInfo {
        self.analyze_dest(dest)
    }

    fn analyze_dest(&self, dest: BasicBlockId) -> TerminatorInfo {
        match &self.scope {
            Some(scope) => {
                if Some(dest) == scope.loop_header_recursive() {
                    TerminatorInfo::Continue
                } else if Some(dest) == scope.loop_exit_recursive() {
                    TerminatorInfo::Break
                } else if Some(dest) == scope.if_merge_block() {
                    TerminatorInfo::ToMergeBlock
                } else {
                    TerminatorInfo::FallThrough(dest)
                }
            }

            None => TerminatorInfo::FallThrough(dest),
        }
    }
}

struct Scope {
    kind: ScopeKind,
    parent: Option<Box<Scope>>,
}

#[derive(Debug, Clone, Copy)]
enum ScopeKind {
    Loop {
        lp: LoopId,
        header: BasicBlockId,
        exit: Option<BasicBlockId>,
    },
    If {
        merge_block: Option<BasicBlockId>,
    },
}

impl Scope {
    fn loop_recursive(&self) -> Option<LoopId> {
        match self.kind {
            ScopeKind::Loop { lp, .. } => Some(lp),
            _ => self.parent.as_ref()?.loop_recursive(),
        }
    }

    fn loop_header(&self) -> Option<BasicBlockId> {
        match self.kind {
            ScopeKind::Loop { header, .. } => Some(header),
            _ => None,
        }
    }

    fn loop_header_recursive(&self) -> Option<BasicBlockId> {
        match self.kind {
            ScopeKind::Loop { header, .. } => Some(header),
            _ => self.parent.as_ref()?.loop_header_recursive(),
        }
    }

    fn loop_exit_recursive(&self) -> Option<BasicBlockId> {
        match self.kind {
            ScopeKind::Loop { exit, .. } => exit,
            _ => self.parent.as_ref()?.loop_exit_recursive(),
        }
    }

    fn if_merge_block(&self) -> Option<BasicBlockId> {
        match self.kind {
            ScopeKind::If { merge_block } => merge_block,
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum TerminatorInfo {
    If {
        cond: ValueId,
        then: Box<TerminatorInfo>,
        else_: Box<TerminatorInfo>,
        merge_block: Option<BasicBlockId>,
    },
    ToMergeBlock,
    Continue,
    Break,
    FallThrough(BasicBlockId),
    NormalInst(InstId),
}

#[cfg(test)]
mod tests {
    use fe_mir::ir::{body_builder::BodyBuilder, inst::InstKind, FunctionId, SourceInfo, TypeId};

    use super::*;

    fn body_builder() -> BodyBuilder {
        BodyBuilder::new(FunctionId(0), SourceInfo::dummy())
    }

    fn serialize_func_body(func: &mut FunctionBody) -> impl Iterator<Item = StructuralInst> {
        InstSerializer::new(func).serialize().into_iter()
    }

    fn expect_if(
        inst: StructuralInst,
    ) -> (
        impl Iterator<Item = StructuralInst>,
        impl Iterator<Item = StructuralInst>,
    ) {
        match inst {
            StructuralInst::If { then, else_, .. } => (then.into_iter(), else_.into_iter()),
            _ => panic!("expect if inst"),
        }
    }

    fn expect_for(inst: StructuralInst) -> impl Iterator<Item = StructuralInst> {
        match inst {
            StructuralInst::For { body } => body.into_iter(),
            _ => panic!("expect if inst"),
        }
    }

    fn expect_break(inst: StructuralInst) {
        assert!(matches!(inst, StructuralInst::Break))
    }

    fn expect_continue(inst: StructuralInst) {
        assert!(matches!(inst, StructuralInst::Continue))
    }

    fn expect_return(func: &FunctionBody, inst: &StructuralInst) {
        match inst {
            StructuralInst::Inst(inst) => {
                assert!(matches!(
                    func.store.inst_data(*inst).kind,
                    InstKind::Return { .. }
                ))
            }
            _ => panic!("expect return"),
        }
    }

    #[test]
    fn if_non_merge() {
        // +------+     +-------+
        // | then | <-- |  bb0  |
        // +------+     +-------+
        //                |
        //                |
        //                v
        //              +-------+
        //              | else_ |
        //              +-------+
        let mut builder = body_builder();

        let then = builder.make_block();
        let else_ = builder.make_block();

        let dummy_ty = TypeId(0);
        let v0 = builder.make_imm_from_bool(true, dummy_ty);
        let unit = builder.make_unit(dummy_ty);

        builder.branch(v0, then, else_, SourceInfo::dummy());

        builder.move_to_block(then);
        builder.ret(unit, SourceInfo::dummy());

        builder.move_to_block(else_);
        builder.ret(unit, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let (mut then, mut else_) = expect_if(order.next().unwrap());
        expect_return(&func, &then.next().unwrap());
        assert!(then.next().is_none());
        expect_return(&func, &else_.next().unwrap());
        assert!(else_.next().is_none());

        assert!(order.next().is_none());
    }

    #[test]
    fn if_merge() {
        // +------+     +-------+
        // | then | <-- |  bb0  |
        // +------+     +-------+
        //   |            |
        //   |            |
        //   |            v
        //   |          +-------+
        //   |          | else_ |
        //   |          +-------+
        //   |            |
        //   |            |
        //   |            v
        //   |          +-------+
        //   +--------> | merge |
        //              +-------+
        let mut builder = body_builder();

        let then = builder.make_block();
        let else_ = builder.make_block();
        let merge = builder.make_block();

        let dummy_ty = TypeId(0);
        let v0 = builder.make_imm_from_bool(true, dummy_ty);
        let unit = builder.make_unit(dummy_ty);

        builder.branch(v0, then, else_, SourceInfo::dummy());

        builder.move_to_block(then);
        builder.jump(merge, SourceInfo::dummy());

        builder.move_to_block(else_);
        builder.jump(merge, SourceInfo::dummy());

        builder.move_to_block(merge);
        builder.ret(unit, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let (mut then, mut else_) = expect_if(order.next().unwrap());
        assert!(then.next().is_none());
        assert!(else_.next().is_none());

        expect_return(&func, &order.next().unwrap());
        assert!(order.next().is_none());
    }

    #[test]
    fn simple_loop() {
        //    +--------+
        //    |  bb0   | -+
        //    +--------+  |
        //      |         |
        //      |         |
        //      v         |
        //    +--------+  |
        // +> | header |  |
        // |  +--------+  |
        // |    |         |
        // |    |         |
        // |    v         |
        // |  +--------+  |
        // +- | latch  |  |
        //    +--------+  |
        //      |         |
        //      |         |
        //      v         |
        //    +--------+  |
        //    |  exit  | <+
        //    +--------+
        let mut builder = body_builder();

        let header = builder.make_block();
        let latch = builder.make_block();
        let exit = builder.make_block();

        let dummy_ty = TypeId(0);
        let v0 = builder.make_imm_from_bool(true, dummy_ty);
        let unit = builder.make_unit(dummy_ty);

        builder.branch(v0, header, exit, SourceInfo::dummy());

        builder.move_to_block(header);
        builder.jump(latch, SourceInfo::dummy());

        builder.move_to_block(latch);
        builder.branch(v0, header, exit, SourceInfo::dummy());

        builder.move_to_block(exit);
        builder.ret(unit, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let (mut lp, mut empty) = expect_if(order.next().unwrap());

        let mut body = expect_for(lp.next().unwrap());
        let (mut continue_, mut break_) = expect_if(body.next().unwrap());
        assert!(body.next().is_none());

        expect_continue(continue_.next().unwrap());
        assert!(continue_.next().is_none());

        expect_break(break_.next().unwrap());
        assert!(break_.next().is_none());

        assert!(empty.next().is_none());

        expect_return(&func, &order.next().unwrap());
        assert!(order.next().is_none());
    }

    #[test]
    fn loop_with_continue() {
        //    +-----+
        // +- | bb0 |
        // |  +-----+
        // |    |
        // |    |
        // |    v
        // |  +---------------+     +-----+
        // |  |      bb1      | --> | bb3 |
        // |  +---------------+     +-----+
        // |    |      ^    ^         |
        // |    |      |    +---------+
        // |    v      |
        // |  +-----+  |
        // |  | bb4 | -+
        // |  +-----+
        // |    |
        // |    |
        // |    v
        // |  +-----+
        // +> | bb2 |
        //    +-----+
        let mut builder = body_builder();

        let bb1 = builder.make_block();
        let bb2 = builder.make_block();
        let bb3 = builder.make_block();
        let bb4 = builder.make_block();

        let dummy_ty = TypeId(0);
        let v0 = builder.make_imm_from_bool(true, dummy_ty);
        let unit = builder.make_unit(dummy_ty);

        builder.branch(v0, bb1, bb2, SourceInfo::dummy());

        builder.move_to_block(bb1);
        builder.branch(v0, bb3, bb4, SourceInfo::dummy());

        builder.move_to_block(bb3);
        builder.jump(bb1, SourceInfo::dummy());

        builder.move_to_block(bb4);
        builder.branch(v0, bb1, bb2, SourceInfo::dummy());

        builder.move_to_block(bb2);
        builder.ret(unit, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let (mut lp, mut empty) = expect_if(order.next().unwrap());
        assert!(empty.next().is_none());

        let mut body = expect_for(lp.next().unwrap());

        let (mut continue_, mut empty) = expect_if(body.next().unwrap());
        expect_continue(continue_.next().unwrap());
        assert!(continue_.next().is_none());
        assert!(empty.next().is_none());

        let (mut continue_, mut break_) = expect_if(body.next().unwrap());
        expect_continue(continue_.next().unwrap());
        assert!(continue_.next().is_none());
        expect_break(break_.next().unwrap());
        assert!(break_.next().is_none());

        assert!(body.next().is_none());
        assert!(lp.next().is_none());

        expect_return(&func, &order.next().unwrap());
        assert!(order.next().is_none());
    }

    #[test]
    fn loop_with_break() {
        //    +-----+
        // +- | bb0 |
        // |  +-----+
        // |    |
        // |    |           +---------+
        // |    v           v         |
        // |  +---------------+     +-----+
        // |  |      bb1      | --> | bb4 |
        // |  +---------------+     +-----+
        // |    |                     |
        // |    |                     |
        // |    v                     |
        // |  +-----+                 |
        // |  | bb3 |                 |
        // |  +-----+                 |
        // |    |                     |
        // |    |                     |
        // |    v                     |
        // |  +-----+                 |
        // +> | bb2 | <---------------+
        //    +-----+
        let mut builder = body_builder();

        let bb1 = builder.make_block();
        let bb2 = builder.make_block();
        let bb3 = builder.make_block();
        let bb4 = builder.make_block();

        let dummy_ty = TypeId(0);
        let v0 = builder.make_imm_from_bool(true, dummy_ty);
        let unit = builder.make_unit(dummy_ty);

        builder.branch(v0, bb1, bb2, SourceInfo::dummy());

        builder.move_to_block(bb1);
        builder.branch(v0, bb3, bb4, SourceInfo::dummy());

        builder.move_to_block(bb3);
        builder.jump(bb2, SourceInfo::dummy());

        builder.move_to_block(bb4);
        builder.branch(v0, bb1, bb2, SourceInfo::dummy());

        builder.move_to_block(bb2);
        builder.ret(unit, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let (mut lp, mut empty) = expect_if(order.next().unwrap());
        assert!(empty.next().is_none());

        let mut body = expect_for(lp.next().unwrap());

        let (mut break_, mut latch) = expect_if(body.next().unwrap());
        expect_break(break_.next().unwrap());
        assert!(break_.next().is_none());

        let (mut continue_, mut break_) = expect_if(latch.next().unwrap());
        assert!(latch.next().is_none());
        expect_continue(continue_.next().unwrap());
        assert!(continue_.next().is_none());
        expect_break(break_.next().unwrap());
        assert!(break_.next().is_none());

        assert!(body.next().is_none());
        assert!(lp.next().is_none());

        expect_return(&func, &order.next().unwrap());
        assert!(order.next().is_none());
    }

    #[test]
    fn loop_no_guard() {
        // +-----+
        // | bb0 |
        // +-----+
        //   |
        //   |
        //   v
        // +-----+
        // | bb1 | <+
        // +-----+  |
        //   |      |
        //   |      |
        //   v      |
        // +-----+  |
        // | bb2 | -+
        // +-----+
        //   |
        //   |
        //   v
        // +-----+
        // | bb3 |
        // +-----+
        let mut builder = body_builder();

        let bb1 = builder.make_block();
        let bb2 = builder.make_block();
        let bb3 = builder.make_block();

        let dummy_ty = TypeId(0);
        let v0 = builder.make_imm_from_bool(true, dummy_ty);
        let unit = builder.make_unit(dummy_ty);

        builder.jump(bb1, SourceInfo::dummy());

        builder.move_to_block(bb1);
        builder.jump(bb2, SourceInfo::dummy());

        builder.move_to_block(bb2);
        builder.branch(v0, bb1, bb3, SourceInfo::dummy());

        builder.move_to_block(bb3);
        builder.ret(unit, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let mut body = expect_for(order.next().unwrap());
        let (mut continue_, mut break_) = expect_if(body.next().unwrap());
        assert!(body.next().is_none());

        expect_continue(continue_.next().unwrap());
        assert!(continue_.next().is_none());

        expect_break(break_.next().unwrap());
        assert!(break_.next().is_none());

        expect_return(&func, &order.next().unwrap());
        assert!(order.next().is_none());
    }

    #[test]
    fn infinite_loop() {
        // +-----+
        // | bb0 |
        // +-----+
        //   |
        //   |
        //   v
        // +-----+
        // | bb1 | <+
        // +-----+  |
        //   |      |
        //   |      |
        //   v      |
        // +-----+  |
        // | bb2 | -+
        // +-----+
        let mut builder = body_builder();

        let bb1 = builder.make_block();
        let bb2 = builder.make_block();

        builder.jump(bb1, SourceInfo::dummy());

        builder.move_to_block(bb1);
        builder.jump(bb2, SourceInfo::dummy());

        builder.move_to_block(bb2);
        builder.jump(bb1, SourceInfo::dummy());

        let mut func = builder.build();
        let mut order = serialize_func_body(&mut func);

        let mut body = expect_for(order.next().unwrap());
        expect_continue(body.next().unwrap());
        assert!(body.next().is_none());

        assert!(order.next().is_none());
    }
}