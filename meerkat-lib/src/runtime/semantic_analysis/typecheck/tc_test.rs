use super::{Type, TypecheckEnv};
use crate::ast::*;
impl TypecheckEnv {
    pub fn typecheck_action(&mut self, commands: &Vec<ActionStmt>) {
        for command in commands.iter() {
            match command {
                ActionStmt::Do(expr) => {
                    let typ = self.infer_expr(expr);
                    if !self.unify(&typ, &Type::Action) {
                        panic!("do requires action expression");
                    }
                }
                ActionStmt::Assert(expr) => {
                    let typ = self.infer_expr(expr);
                    if !self.unify(&typ, &Type::Bool) {
                        panic!("Assert statement requires bool expression");
                    }
                }
                _ => panic!("not implemented")
            }
        }
    }
}
