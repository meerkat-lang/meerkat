use core::panic;

use super::{Type, TypecheckEnv};
use crate::ast::*;

impl TypecheckEnv {
    pub fn infer_expr(&mut self, expr: &Expr) -> Type {
        use Type::*;
        match expr {
            Expr::Number { val: _ } => Int,
            Expr::Bool { val: _ } => Bool,
            Expr::String {val: _} => String,
            Expr::KeyVal { key, value } => self.infer_expr(&value),
            Expr::Vector { val } => {
                let mut type_vec = Vec::new();
                for el in val {
                    type_vec.push(self.infer_expr(el));
                }
                Type::Vector(type_vec)
            }
            Expr::Variable { ident } => self
                .var_context
                .get(ident)
                .or(self.name_context.get(ident))
                .cloned()
                .expect(&format!("cannot find var {:?} in context", ident)),

            Expr::Unop { op, expr } => match op {
                UnOp::Neg => {
                    let typ = self.infer_expr(expr);
                    if self.unify(&typ, &Int) {
                        Int
                    } else {
                        panic!("cannot unify {:?} and int", typ)
                    }
                }
                UnOp::Not => {
                    let typ = self.infer_expr(expr);
                    if self.unify(&typ, &Bool) {
                        Bool
                    } else {
                        panic!("cannot unify {:?} and bool", typ)
                    }
                }
            },

            Expr::Binop { op, expr1, expr2 } => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                    let typ1 = self.infer_expr(expr1);
                    let typ2 = self.infer_expr(expr2);
                    if !self.unify(&typ1, &Int) {
                        panic!("cannot unify left hand side {:?} and int", typ1)
                    } else if !self.unify(&typ2, &Int) {
                        panic!("cannot unify right hand side {:?} and int", typ2)
                    } else {
                        Int
                    }
                }
                BinOp::Lt | BinOp::Gt => {
                    let typ1 = self.infer_expr(expr1);
                    let typ2 = self.infer_expr(expr2);
                    if !self.unify(&typ1, &Int) {
                        panic!("cannot unify left hand side {:?} and int", typ1)
                    } else if !self.unify(&typ2, &Int) {
                        panic!("cannot unify right hand side {:?} and int", typ2)
                    } else {
                        Bool
                    }
                }

                BinOp::And | BinOp::Or => {
                    let typ1 = self.infer_expr(expr1);
                    let typ2 = self.infer_expr(expr2);
                    if !self.unify(&typ1, &Bool) {
                        panic!("cannot unify left hand side {:?} and bool", typ1)
                    } else if !self.unify(&typ2, &Bool) {
                        panic!("cannot unify right hand side {:?} and bool", typ2)
                    } else {
                        Bool
                    }
                }

                BinOp::Eq => {
                    let typ1 = self.infer_expr(expr1);
                    let typ2 = self.infer_expr(expr2);
                    if !self.unify(&typ1, &typ2) {
                        panic!("cannot unify {:?} and {:?}", typ1, typ2)
                    } else {
                        Bool
                    }
                }
            },

            Expr::If { cond, expr1, expr2 } => {
                let cond_typ = self.infer_expr(cond);
                if !self.unify(&cond_typ, &Bool) {
                    panic!("cannot unify {:?} and bool", cond_typ);
                }
                let typ1 = self.infer_expr(expr1);
                let typ2: Type = self.infer_expr(expr2);
                if !self.unify(&typ1, &typ2) {
                    panic!("cannot unify {:?} and {:?}", typ1, typ2);
                }

                assert!(self.find(&typ1) == self.find(&typ2));
                self.find(&typ1)
            }

            Expr::Func { params, body } => {
                // check params are unique
                let mut param_set = std::collections::HashSet::new();
                for param in params.iter() {
                    if !param_set.insert(param) {
                        panic!("duplicate param name: {}", param);
                    }
                }

                // frozen current context
                let old_context = self.var_context.clone();

                // first generate type variables for param, update context
                let mut param_types = vec![];
                for param in params.iter() {
                    let typ = self.gen_typevar();
                    self.var_context.insert(param.clone(), typ.clone());
                    param_types.push(typ);
                }

                // type infer func body mutates acc_subst
                let mut ret_typ = self.infer_expr(&body);

                // generate function type signature of canonical form
                for param_typ in param_types.iter_mut() {
                    *param_typ = self.find(param_typ);
                }
                ret_typ = self.find(&ret_typ);

                // restore old context
                self.var_context = old_context;

                Fun(param_types, Box::new(ret_typ))
            }

            Expr::FuncApply { func, args } => {
                let func_typ = self.infer_expr(func);
                if let Type::Fun(arg_typs, ret_typ) = func_typ {
                    if arg_typs.len() != args.len() {
                        panic!("wrong number of argument to apply");
                    } else {
                        for (i, arg) in args.iter().enumerate() {
                            let typ_i: &Type = &arg_typs[i];
                            let typ_i_actual = self.infer_expr(arg);
                            if !self.unify(typ_i, &typ_i_actual) {
                                panic!(
                                    "cannot unify {:?}th argument, 
                                    expect {:?} got {:?}",
                                    i, typ_i, typ_i_actual
                                )
                            }
                        }
                        self.find(&ret_typ)
                    }
                } else {
                    let ret_typ = self.gen_typevar();
                    let arg_typs = args.iter().map(|a| self.infer_expr(a)).collect();

                    let func_typ_actual = Type::Fun(arg_typs, Box::new(ret_typ.clone()));

                    if !self.unify(&func_typ, &func_typ_actual) {
                        panic!(
                            "cannot unify function type, expected {:?} got {:?}",
                            func_typ, func_typ_actual
                        );
                    }

                    self.find(&ret_typ)
                }
            }

            // more todo on Action type
            Expr::Action { assns , inserts} => {
                assns.iter().for_each(|assn| self.typecheck_assn(assn));
                inserts.iter().for_each(|insert| self.typecheck_insert(insert));

                Action
            }
            Expr::Select { table_name, column_names, where_clause } => {
                let schema = {
                    let table_type = self.var_context.get(table_name);    // check if table exists and extract schema
                    match table_type {
                        Some(Type::Table(fields)) => fields.clone(),
                        _ => panic!("Table {} for selection not found or not a table type", table_name),
                    }
                };
                let field_names: Vec<_> = schema.iter().map(|field| field.name.clone()).collect();   // get column names and check if columns to be selected exist
                for column_name in column_names {
                    if !field_names.contains(column_name) {
                        panic!("{} field not found in table {}", column_name, table_name);
                    }
                }

                let cond_type = self.infer_expr(where_clause);
                
                if cond_type != Type::Bool {
                    panic!("Select where clause must be boolean, got {}", cond_type);
                }
                Type::Table(schema)    
            }
            Expr::TableColumn { table_name, column_name } => {
                // Look up table type in context
                let found_table = self.var_context.get(table_name);
                match found_table {
                    Some(Type::Table(fields)) => {
                        let found_column = fields.iter().find(|field| &field.name == column_name);
                        match found_column {
                            Some(field) => match field.type_ {
                                DataType::Bool => Type::Bool,
                                DataType::Number => Type::Int,
                                DataType::String => Type::String,
                            },
                            None => panic!("Column {} not found in table {}", column_name, table_name),
                        }
                    }
                    _ => panic!("Table {} for TableColumn not found or not a table type", table_name),
                }
            }
            Expr::Table {schema, records } => Table(schema.to_vec()),
            Expr::Fold { args } => {
                if args.len()!=3 {
                    panic!("Fold expects 3 arguments, got {} arguments", args.len());
                }
                
                let column_type = self.infer_expr(&args[0]);
                let func_type = self.infer_expr(&args[1]);
                let accum_type = self.infer_expr(&args[2]);

                if let Expr::TableColumn { .. } = &args[0] {     // Maybe later we can have a tablecolumn type for typechecking here
                    self.infer_expr(&args[0]);
                } else {         
                    panic!("First argument should be iterator (column)");
                }

                match func_type {
                    Type::Fun(params, ret_type) => {
                        if !self.unify(&accum_type, &*ret_type) {
                            panic!("Accumulator type should be the same as function return type, expected {}, got {}", &*ret_type, &accum_type);
                        }
                        for param in params {
                            if !self.unify(&param, &column_type) {
                                panic!("Column type should be the same as function argument type, expected {}, got {}", &param, &column_type);
                            }
                        }
                    },
                    _ => panic!("Second argument must be function type")
                }
                self.find(&accum_type)
                
            },
            
        }
}
}

// TODO List
// (priority) implement statics for actions
// 1. more efficient implementation of var context
// 2. add language feature: let binding
