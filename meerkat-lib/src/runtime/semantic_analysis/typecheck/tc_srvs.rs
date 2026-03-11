use crate::ast::Decl;

use super::TypecheckEnv;

impl TypecheckEnv {
    pub fn typecheck_service(&mut self, decls: &Vec<Decl>) {
        for decl in decls {
            self.typecheck_decl(decl);
        }
    }
}
