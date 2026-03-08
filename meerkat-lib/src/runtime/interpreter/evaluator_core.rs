use crate::ast::{
    Assn, DataType, Decl, Entry, Expr, Field, Insert, Prog, Record, ReplCmd, Service, Test,
};
use std::{
    collections::{HashMap, HashSet},
    env,
    fmt::Display,
};

use crate::runtime::{manager::Manager, message::Msg};


#[derive(Debug, Clone)]
pub enum Val {
    Number(i32),
    Bool(bool),
    Action(Vec<Assn>, Vec<Insert>),
    Func(Vec<String>, Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct Evaluator {
    pub var_id_cnt: i32,
    pub reactive_names: HashSet<String>,

    /// context are modally separated into, we didn't use lambda expr var
    /// as we subst directly in place when we eval expr
    /// reactive def/var name -> val
    pub reactive_name_to_vals: HashMap<String, Expr>,
    /// lambda expr var name -> val
    /// exprvar_name_to_val: HashMap<String, Expr>,
    pub def_name_to_exprs: HashMap<String, Expr>,

}

impl Evaluator {
    pub fn new(reactive_name_to_vals: HashMap<String, Expr>) -> Evaluator {
        Evaluator {
            var_id_cnt: 0,
            reactive_names: HashSet::new(),
            reactive_name_to_vals,
            def_name_to_exprs: HashMap::new(),
        }
    }
}

/// used for def actor eval their expression
pub fn eval_def_expr(
    def_expr: &Expr,
    env: &HashMap<String, Expr>,
) -> Expr {
    let mut eval = Evaluator::new(env.clone());
    let mut evaled_expr = def_expr.clone();
    eval.eval_expr(&mut evaled_expr);
    evaled_expr
}

/// used for manager eval assns when action is triggered
pub fn eval_assns(assns: &Vec<Assn>, env: HashMap<String, Expr>) -> Vec<Assn> {
    let mut eval = Evaluator::new(env);
    let mut evaled_assns = assns.clone();
    for assn in evaled_assns.iter_mut() {
        eval.eval_assn(assn);
    }

    evaled_assns
}

/// used for initial eval of all declarations in a service
pub fn eval_srv(srv: &Service) -> Evaluator {
    let mut srv = srv.clone();
    let mut eval = Evaluator::new(HashMap::new());
    for decl in srv.decls.iter_mut() {
        let _ = eval.eval_decl(decl);
        // println!("{}", decl);
    }
    eval
}

impl From<Val> for Expr {
    fn from(val: Val) -> Self {
        match val {
            Val::Number(i) => Expr::Number { val: i },
            Val::Bool(b) => Expr::Bool { val: b },
            Val::Action(assns, inserts) => Expr::Action { assns, inserts },
            Val::Func(params, body) => Expr::Func { params, body },
        }
    }
}

impl Display for Val {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", Expr::from(self.clone()))
    }
}
