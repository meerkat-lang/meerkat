use crate::ast::{Assn, DataType, Decl, Insert, Expr, Field};

use std::collections::HashSet;
use super::TypecheckEnv;
use crate::static_analysis::typecheck::Type;

impl TypecheckEnv {
    pub fn typecheck_decl(&mut self, decl: &Decl) {
        match decl {
            Decl::Import { srv_name } => todo!(),
            Decl::VarDecl { name, val } => {
                let typ = self.infer_expr(&val);
                self.name_context.insert(name.clone(), typ);
            }
            Decl::DefDecl { name, val, is_pub } => {
                let typ = self.infer_expr(&val);
                self.name_context.insert(name.clone(), typ);
            }
            Decl::TableDecl { name, fields } =>  {
                let mut names = HashSet::new();
                for field in fields {
                    if !names.insert(field.name.clone()) {
                        panic!("Duplicate names found in table {}", name)
                    }
                }
                self.var_context.insert(name.clone(), Type::Table(fields.to_vec()));
            }
        }
    }

    pub fn typecheck_assn(&mut self, assn: &Assn) {
        let dest_typ = self
            .name_context
            .get(&assn.dest)
            .cloned()
            .expect(&format!("cannot find {:?} in var context", assn.dest));
        let src_typ = self.infer_expr(&assn.src);

        if !self.unify(&dest_typ, &src_typ) {
            panic!(
                "cannot unify left {:?} and right {:?} in assign",
                dest_typ, src_typ
            );
        }
    }

    pub fn typecheck_insert(&mut self, insert: &Insert) {
        let found_type = self.var_context.get(&insert.table_name).cloned();
        match found_type {
            None => panic!("Table {} for insertion not found", insert.table_name),
            Some(table_type) => {
                match &table_type {
                    Type::Table(schema) => {
                        match &insert.row {
                            Expr::Vector { val: keyvals } => {
                                if keyvals.len()!= schema.len() {
                                    panic!("Entries in the row do not match the table {} schema", insert.table_name);
                                }
                                for keyval in keyvals {
                                    match keyval {
                                        Expr::KeyVal { key, value} => {
                                            
                                            let field_type = schema.iter().find(|f| f.name == *key)
                                                .unwrap_or_else(|| panic!("Field '{}' not found in table '{}' schema", key, insert.table_name));
                                            let expected_type = match field_type.type_ {
                                                DataType::Bool => Type::Bool,
                                                DataType::Number => Type::Int,
                                                DataType::String => Type::String,
                                            };
                                            let inferred_type = self.infer_expr(&*value);
                                            if !self.unify(&inferred_type, &expected_type) {
                                                panic!("Data type of entry '{}' does not match the schema, expected {:?}, got {:?}", key, expected_type, inferred_type);
                                            }
                                        }
                                        _ => panic!("Non keyval found")
                                    }
                                }
                            }
                            expr => {
                                let inferred_type = self.infer_expr(expr);
                                let expected_type = Type::Table(schema.clone());
                                if !self.unify(&inferred_type, &expected_type) {
                                    panic!("Insert expression type does not match table schema, expected {:?}, got {:?}", expected_type, inferred_type);
                                }
                            }
                        }
                    }
                    // If table_name is a parameter (type variable), unify row type with it
                    other_type => {
                        let row_type = self.infer_expr(&insert.row);
                        if !self.unify(&row_type, other_type) {
                            panic!("Insert row type does not match parameter type for '{}', expected {:?}, got {:?}", insert.table_name, other_type, row_type);
                        }
                    }
                }
            }
        }
    }
}

// todo: assign checking
// todo: name checking