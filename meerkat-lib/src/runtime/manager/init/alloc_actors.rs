use core::panic;
use log::info;
use std::collections::{HashMap, HashSet};
use std::error::Error;

use kameo::{prelude::*, spawn};

use crate::runtime::manager::Manager;
use crate::runtime::table_actor::TableActor;
use crate::runtime::TestId;
use crate::{
    ast::{Expr, Prog, Service, Decl, Field},
    runtime::{def_actor::DefActor, evaluator::eval_srv, message::Msg, var_actor::VarActor},
    static_analysis::var_analysis::calc_dep_srv,
};

impl Manager {
    pub async fn alloc_var_actor(&mut self, name: &String, val: Expr) {
        let actor_ref = spawn(VarActor::new(name.clone(), val));
        self.varname_to_actors.insert(name.clone(), actor_ref);
    }

    /// current impl of alloc_def_actor rely on a centralized manager
    /// preprocess all information
    ///
    /// should change to distributed and incrementally allocate def actor
    /// to accommodate for code update
    pub async fn alloc_def_actor(
        &mut self,
        name: &String,
        expr: Expr,
    ) -> Result<ActorRef<DefActor>, Box<dyn Error>> {
        // calculate all information for def actor, default is used for non-source code part, like assertions
        let def_args = self.dep_graph.get(name).map_or_else(
            || expr.free_var(&self.evaluator.reactive_names, &HashSet::new()), // incrementally calculated
            |def_args| def_args.clone(), // precalculated by centralized manager
        );

        let def_arg_to_vals = def_args
            .iter()
            .map(|name| {
                if self.evaluator.reactive_name_to_vals.contains_key(name) {
                    Ok((
                        name.clone(),
                        self.evaluator
                            .reactive_name_to_vals
                            .get(name)
                            .expect(&format!(
                                "DefActor alloc: var/def is not initialized: {}",
                                name
                            ))
                            .clone(),
                    ))
                } else if self.evaluator.reactive_name_to_vals.contains_key(name) {
                    let Some(table) = self.evaluator.reactive_name_to_vals.get(name) else {
                        return Err(format!("Table not found: {}", name));
                    };
                    if let Expr::Table {schema, records } = table {
                        Ok((
                        name.clone(),
                        Expr::Table {
                            schema: schema.clone(),
                            records: records.clone(),
                        },
                    ))
                    } else {
                        panic!("Table not found");
                    }
                    
                } else {
                    Err(format!("Var/table/def not initialized: {}", name))
                }
            })
            .collect::<Result<HashMap<String, Expr>, String>>()?;

        let def_arg_to_vars = def_args
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    self.dep_tran_vars
                        .get(name)
                        .expect(&format!(
                            "DefActor alloc: var/def is not initialized: {}",
                            name
                        ))
                        .clone(),
                )
            })
            .collect::<HashMap<String, HashSet<String>>>();

        let mut val = expr.clone();
        let _ = self.evaluator.eval_expr(&mut val);

        let actor_ref = spawn(DefActor::new(
            name.clone(),
            expr,
            val,
            def_arg_to_vals,
            def_arg_to_vars,
        ));
        self.defname_to_actors
            .insert(name.clone(), actor_ref.clone());

        // subscribe to its dependencies
        // println!("{} subscribe to {:?}", name, def_args);
        for name in def_args.iter() {
            // println!("{}", name);
            // synchronously wait for response
            let back_msg = self
                .ask_to_name(
                    name,
                    Msg::Subscribe {
                        from_name: name.clone(),
                        from_addr: actor_ref.clone(),
                    },
                )
                .await?;

            if !matches!(back_msg, Msg::SubscribeGranted { .. }) {
                panic!("Service alloc: receive wrong message type during subscription");
            }

            actor_ref.tell(back_msg).await?;
        }

        Ok(actor_ref)
    }

    pub async fn alloc_table_actor(&mut self, name: &String, val: Expr) {
        info!("spawning table actor");
        let actor_ref = spawn(TableActor::new(name.clone(), val));
        self.tablename_to_actors.insert(name.clone(), actor_ref);
    }
}

// later when we want to fully implement services and code update
// here are some todos
// - how to process remote var/def values
// - incrementally update dep_graph and related data structures in srv manager
