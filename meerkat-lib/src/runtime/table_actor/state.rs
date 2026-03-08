use core::panic;

use crate::ast::{ Expr, Insert};

#[derive(Debug, Clone)]
pub enum TableValueState {
    Val(Expr),
}


impl TableValueState {
    pub fn new(val: Expr) -> TableValueState {
        TableValueState::Val(val)
    }

    pub fn update(&mut self, insert: &Insert) {
        assert!(matches!(insert.row, Expr::Vector { val: _ }));
        if let TableValueState::Val(Expr::Table { records, .. }) = self {
            records.push(insert.row.clone());
        } else {
            panic!("Not a table");
        };
    }
}

impl Into<Expr> for TableValueState {
    fn into(self) -> Expr {
        match self {
            TableValueState::Val(val) => val
        }
    }
}
