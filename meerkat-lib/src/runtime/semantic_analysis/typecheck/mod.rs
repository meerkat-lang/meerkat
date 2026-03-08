//! Hindley-Milner type inference
//! resources:
//! https://course.ccs.neu.edu/cs4410sp19/lec_type-inference_notes.html,
//! our previous implementation
//! the union-find algorithm

mod tc_expr;
mod tc_srvs;
mod tc_stmt;
mod tc_test;
mod utils;

use std::{collections::HashMap, fmt::Display};

use crate::ast::{Prog, DataType, Field};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    Bool,
    String,
    Vector(Vec<Type>),
    Unit,
    Action,

    Fun(Vec<Type>, Box<Type>), // for instantiated type

    TypVar(String),
    Table(Vec<Field>),
    Row,
}

/// Type Scheme represents polymorphic types,
/// e.g. \forall a, b, c in (a * b) -> c
// pub struct TypeScheme {
//     args: Vec<Type>,
//     body: Type,
// }

// impl TypeScheme {
//     fn free_var(&self) -> HashSet<String> {
//         todo!()
//     }
// }

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "int"),
            Type::Bool => write!(f, "bool"),
            Type::String => write!(f, "string"),
            Type::Vector( .. ) => write!(f, "vector"),
            Type::Unit => write!(f, "unit"),
            Type::Action => write!(f, "action"),
            Type::Fun(args, ret) => {
                let joined = args
                    .iter()
                    .map(|t| format!("{}", t))
                    .collect::<Vec<_>>()
                    .join(",");
                if args.len() > 1 {
                    write!(f, "({})->{}", joined, ret)
                } else {
                    write!(f, "{}->{}", joined, ret)
                }
            }
            Type::TypVar(name) => write!(f, "{}", name),
            Type::Table( schema) => write!(f, "table {:?}", schema),
            Type::Row => write!(f, "row")
        }
    }
}

pub struct TypecheckEnv {
    pub var_context: HashMap<String, Type>, // Expr::Var to type, todo: change this to more efficient stack of hashmap
    pub name_context: HashMap<String, Type>, // reactive name to type
    // pub var_to_typ_scheme: HashMap<String, TypeScheme>,

    // counter to generate new type var
    pub typevar_id: u64,
    // Type::var to type (canonical form)
    pub acc_subst: HashMap<String, Type>,
    
}

impl Display for TypecheckEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "------------------\n")?;
        for (var_name, var_typ) in self.var_context.iter() {
            write!(f, "{}: {}\n", var_name, var_typ)?;
        }
        write!(f, "------------------\n")
    }
}

pub fn typecheck_prog(prog: &Prog) {
    // each service has its own type environment
    let mut srv_to_type_env = HashMap::new();

    for srvs in prog.services.iter() {
        let mut typ_env = TypecheckEnv::new();
        typ_env.typecheck_service(srvs);
        print!("service: {:?}\n {}", srvs.name, typ_env);

        srv_to_type_env.insert(srvs.name.clone(), typ_env);
    }

    for test in prog.tests.iter() {
        srv_to_type_env
            .get_mut(&test.name)
            .expect(&format!(
                "Test: test instantiate a non-existing service {:?}",
                test.name
            ))
            .typecheck_test(test);
    }
}
