// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use build::{BlockAnd, BlockAndExtension, Builder};
use build::scope::LoopScope;
use hair::*;
use rustc::middle::region::CodeExtent;
use rustc::mir::repr::*;
use rustc::hir;
use syntax::codemap::Span;

impl<'a,'tcx> Builder<'a,'tcx> {
    pub fn ast_block(&mut self,
                     destination: &Lvalue<'tcx>,
                     mut block: BasicBlock,
                     ast_block: &'tcx hir::Block)
                     -> BlockAnd<()> {
        let Block { extent, span, stmts, expr } = self.hir.mirror(ast_block);
        self.in_scope(extent, block, move |this, _| {
            // This convoluted structure is to avoid using recursion as we walk down a list
            // of statements. Basically, the structure we get back is something like:
            //
            //    let x = <init> in {
            //       expr1;
            //       let y = <init> in {
            //           expr2;
            //           expr3;
            //           ...
            //       }
            //    }
            //
            // The let bindings are valid till the end of block so all we have to do is to pop all
            // the let-scopes at the end.
            //
            // First we build all the statements in the block.
            let mut let_extent_stack = Vec::with_capacity(8);
            for stmt in stmts {
                let Stmt { span: _, kind } = this.hir.mirror(stmt);
                match kind {
                    StmtKind::Expr { scope, expr } => {
                        unpack!(block = this.in_scope(scope, block, |this, _| {
                            let expr = this.hir.mirror(expr);
                            this.stmt_expr(block, expr)
                        }));
                    }
                    StmtKind::Let { remainder_scope, init_scope, pattern, initializer } => {
                        let remainder_scope_id = this.push_scope(remainder_scope, block);
                        let_extent_stack.push(remainder_scope);
                        unpack!(block = this.in_scope(init_scope, block, move |this, _| {
                            // FIXME #30046                              ^~~~
                            if let Some(init) = initializer {
                                this.expr_into_pattern(block, remainder_scope_id, pattern, init)
                            } else {
                                this.declare_bindings(remainder_scope_id, &pattern);
                                block.unit()
                            }
                        }));
                    }
                }
            }
            // Then, the block may have an optional trailing expression which is a “return” value
            // of the block.
            if let Some(expr) = expr {
                unpack!(block = this.into(destination, block, expr));
            } else {
                // FIXME(#31472)
                let scope_id = this.innermost_scope_id();
                this.cfg.push_assign_unit(block, scope_id, span, destination);
            }
            // Finally, we pop all the let scopes before exiting out from the scope of block
            // itself.
            for extent in let_extent_stack.into_iter().rev() {
                unpack!(block = this.pop_scope(extent, block));
            }
            block.unit()
        })
    }

    pub fn stmt_expr(&mut self, mut block: BasicBlock, expr: Expr<'tcx>) -> BlockAnd<()> {
        let this = self;
        let expr_span = expr.span;
        let scope_id = this.innermost_scope_id();
        // Handle a number of expressions that don't need a destination at all. This
        // avoids needing a mountain of temporary `()` variables.
        match expr.kind {
            ExprKind::Scope { extent, value } => {
                let value = this.hir.mirror(value);
                this.in_scope(extent, block, |this, _| this.stmt_expr(block, value))
            }
            ExprKind::Assign { lhs, rhs } => {
                let lhs = this.hir.mirror(lhs);
                let scope_id = this.innermost_scope_id();
                let lhs_span = lhs.span;
                let lhs_ty = lhs.ty;

                let lhs_needs_drop = this.hir.needs_drop(lhs_ty);

                // Note: we evaluate assignments right-to-left. This
                // is better for borrowck interaction with overloaded
                // operators like x[j] = x[i].

                // Generate better code for things that don't need to be
                // dropped. We need the temporary as_operand generates
                // so we can clean up the data if evaluating the LHS unwinds,
                // but if the LHS (and therefore the RHS) doesn't need
                // unwinding, we just translate directly to an rvalue instead.
                let rhs = if lhs_needs_drop {
                    let op = unpack!(block = this.as_operand(block, rhs));
                    Rvalue::Use(op)
                } else {
                    unpack!(block = this.as_rvalue(block, rhs))
                };

                let lhs = unpack!(block = this.as_lvalue(block, lhs));
                unpack!(block = this.build_drop(block, lhs_span, lhs.clone(), lhs_ty));
                this.cfg.push_assign(block, scope_id, expr_span, &lhs, rhs);
                block.unit()
            }
            ExprKind::AssignOp { op, lhs, rhs } => {
                // FIXME(#28160) there is an interesting semantics
                // question raised here -- should we "freeze" the
                // value of the lhs here?  I'm inclined to think not,
                // since it seems closer to the semantics of the
                // overloaded version, which takes `&mut self`.  This
                // only affects weird things like `x += {x += 1; x}`
                // -- is that equal to `x + (x + 1)` or `2*(x+1)`?

                // As above, RTL.
                let rhs = unpack!(block = this.as_operand(block, rhs));
                let lhs = unpack!(block = this.as_lvalue(block, lhs));

                // we don't have to drop prior contents or anything
                // because AssignOp is only legal for Copy types
                // (overloaded ops should be desugared into a call).
                this.cfg.push_assign(block, scope_id, expr_span, &lhs,
                                     Rvalue::BinaryOp(op,
                                                      Operand::Consume(lhs.clone()),
                                                      rhs));

                block.unit()
            }
            ExprKind::Continue { label } => {
                this.break_or_continue(expr_span, label, block,
                                       |loop_scope| loop_scope.continue_block)
            }
            ExprKind::Break { label } => {
                this.break_or_continue(expr_span, label, block, |loop_scope| {
                    loop_scope.might_break = true;
                    loop_scope.break_block
                })
            }
            ExprKind::Return { value } => {
                block = match value {
                    Some(value) => unpack!(this.into(&Lvalue::ReturnPointer, block, value)),
                    None => {
                        this.cfg.push_assign_unit(block, scope_id,
                                                  expr_span, &Lvalue::ReturnPointer);
                        block
                    }
                };
                let extent = this.extent_of_return_scope();
                let return_block = this.return_block();
                this.exit_scope(expr_span, extent, block, return_block);
                this.cfg.start_new_block().unit()
            }
            _ => {
                let expr_span = expr.span;
                let expr_ty = expr.ty;
                let temp = this.temp(expr.ty.clone());
                unpack!(block = this.into(&temp, block, expr));
                unpack!(block = this.build_drop(block, expr_span, temp, expr_ty));
                block.unit()
            }
        }
    }

    fn break_or_continue<F>(&mut self,
                            span: Span,
                            label: Option<CodeExtent>,
                            block: BasicBlock,
                            exit_selector: F)
                            -> BlockAnd<()>
        where F: FnOnce(&mut LoopScope) -> BasicBlock
    {
        let (exit_block, extent) = {
            let loop_scope = self.find_loop_scope(span, label);
            (exit_selector(loop_scope), loop_scope.extent)
        };
        self.exit_scope(span, extent, block, exit_block);
        self.cfg.start_new_block().unit()
    }
}
