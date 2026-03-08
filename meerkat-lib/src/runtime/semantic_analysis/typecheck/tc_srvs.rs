use crate::ast::Service;

use super::TypecheckEnv;

impl TypecheckEnv {
    pub fn typecheck_service(&mut self, srvs: &Service) {
        for stmt in srvs.decls.iter() {
            self.typecheck_decl(stmt);
        }
    }
}
