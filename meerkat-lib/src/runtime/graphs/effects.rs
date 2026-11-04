//! Immediate effects of declarations, from which the dependency graph is built
//!
//! A member's code reads another member when evaluating that code does
//! (§2.2 of the semantics): constructing a lambda or an action reads nothing
//! (C-Abs, C-Action), applying a function reads what its body reads (C-App),
//! and a conditional reads what either branch reads (C-If). Function values
//! are followed through locals, parameters, members and results, so the reads
//! latent in a function body are attributed to the caller, not to the member
//! holding the function
//!
//! The analysis never fails. The functions a member may hold come from its
//! code or, once the runtime holds them, from its values: a var holds what
//! its initializer or an action stored in it, and a remote member what its
//! owner published to the defs here that read it, or what a transaction
//! initializing one of them read. Applying a function whose code is not known
//! here reads nothing; the runtime analyzes the code when it arrives, before
//! any def can apply it

use super::ServiceGraphs;
use crate::ast::{Decl, Expr, Stmt, Value};
use crate::runtime::interner::Symbol;
use crate::runtime::limits::MAX_SCOPE_DEPTH;
use crate::runtime::tt::Param;
use petgraph::graphmap::DiGraphMap;
use std::collections::{BTreeSet, HashMap, HashSet};

/// A member, as `(service, member)`
pub(crate) type Member = (Symbol, Symbol);

/// A function a value may be when it is applied: the lambda at this index of
/// `Effects::lambdas`, or `UNKNOWN`
type Function = usize;

/// A function whose code is not known here
const UNKNOWN: Function = usize::MAX;

/// The functions a value may be; empty for a value that is not a function
type Functions = BTreeSet<Function>;

type Locals = Vec<(Symbol, Functions)>;

/// The members evaluating some code reads: those its own code names, and
/// those the functions it applies read
#[derive(Default)]
struct Reads {
    direct: HashSet<Member>,
    activated: HashSet<Member>,
}

impl Reads {
    fn all(&self) -> impl Iterator<Item = Member> + '_ {
        self.direct.iter().chain(&self.activated).copied()
    }
}

/// A function applied to the functions each argument may be
type Application = (Function, Vec<Functions>);

/// A lambda of the program, applied in its service to its captured locals
#[derive(Clone)]
struct Lambda<'a> {
    service: Symbol,
    params: &'a [Param],
    body: &'a Expr,
    env: Locals,
}

/// The reads of the effective declarations of one program
#[derive(Default)]
pub(crate) struct Effects<'a> {
    services: HashMap<Symbol, &'a [Decl]>,
    /// Values remote owners published for their members, analyzed in place of
    /// the members' declarations. While a new version is on its way to some
    /// readers, a member may hold any version they hold
    published: HashMap<Member, Vec<&'a Value>>,
    /// Each lambda met, at the index that is its `Function`. Lambdas are the
    /// same function when they are the same code with the same captured locals
    lambdas: Vec<Lambda<'a>>,
    functions: HashMap<(Symbol, *const Expr, Locals), Function>,
    /// The functions each member may hold, once computed
    values: HashMap<Member, Functions>,
    /// Members whose value is being computed
    computing: HashSet<Member>,
    /// Applications being analyzed, each with the depth of the outermost of
    /// these it reentered, directly or through the applications it made
    applying: Vec<(Application, usize)>,
    /// The reads and result of each application whose analysis did not
    /// reenter an application enclosing it
    applied: HashMap<Application, (HashSet<Member>, Functions)>,
}

impl<'a> Effects<'a> {
    pub(crate) fn new(program: &'a [Stmt], published: HashMap<Member, Vec<&'a Value>>) -> Self {
        let services = super::analysis::service_declarations(program)
            .into_iter()
            .filter_map(|(name, stmt)| match stmt {
                Stmt::Service { decls, .. } => Some((name, decls.as_slice())),
                _ => None,
            })
            .collect();
        Effects {
            services,
            published,
            ..Default::default()
        }
    }

    /// The dependency graphs of `decls`, declarations of `service`: an edge or
    /// cross-service dependency for each member a declaration's code reads
    pub(crate) fn graphs(&mut self, service: Symbol, decls: &'a [Decl]) -> ServiceGraphs {
        let mut graphs = ServiceGraphs::new();
        for decl in decls {
            let (Decl::VarDecl { name, .. }
            | Decl::DefDecl { name, .. }
            | Decl::TableDecl { name, .. }) = decl;
            match decl {
                Decl::VarDecl { .. } => graphs.vars.insert(*name),
                Decl::DefDecl { .. } => graphs.defs.insert(*name),
                Decl::TableDecl { .. } => graphs.tables.insert(*name),
            };
            graphs.reactive_graph.add_node(*name);
        }
        for decl in decls {
            let Some((name, reads)) = self.member_reads(service, decl) else {
                continue;
            };
            for (owner, member) in reads.all() {
                if owner != service {
                    graphs
                        .cross_deps
                        .entry(name)
                        .or_default()
                        .insert((owner, member));
                } else if graphs.reactive_graph.contains_node(member) {
                    graphs.reactive_graph.add_edge(member, name, ());
                }
            }
        }
        graphs
    }

    /// What evaluating the code of `decl`, a declaration of `service`, reads
    fn member_reads(&mut self, service: Symbol, decl: &'a Decl) -> Option<(Symbol, Reads)> {
        let (Decl::VarDecl { name, val, .. } | Decl::DefDecl { name, val, .. }) = decl else {
            return None;
        };
        let mut reads = Reads::default();
        self.visit(val, service, &[], &mut reads);
        Some((*name, reads))
    }

    fn decl(&self, (service, member): Member) -> Option<&'a Decl> {
        self.services.get(&service)?.iter().find(|decl| {
            let (Decl::VarDecl { name, .. }
            | Decl::DefDecl { name, .. }
            | Decl::TableDecl { name, .. }) = decl;
            *name == member
        })
    }

    /// Add to `reads` what evaluating `expr` in `service` reads, and return the
    /// functions its value may be
    fn visit(
        &mut self,
        expr: &'a Expr,
        service: Symbol,
        locals: &[(Symbol, Functions)],
        reads: &mut Reads,
    ) -> Functions {
        match expr {
            Expr::Literal { val } => self.literal(val),
            // A name that is not a local is a member of the code's service,
            // declared here or not
            Expr::Variable { name } => match locals.iter().rev().find(|(local, _)| local == name) {
                Some((_, functions)) => functions.clone(),
                None => self.read((service, *name), reads),
            },
            &Expr::MemberAccess {
                service_name,
                member_name,
            } => self.read((service_name, member_name), reads),
            Expr::Func { params, body, .. } => {
                // A closure does not capture a local named like a member of
                // its service, as in `eval`
                let env = locals
                    .iter()
                    .filter(|(local, _)| self.decl((service, *local)).is_none())
                    .cloned()
                    .collect();
                BTreeSet::from([self.function(Lambda {
                    service,
                    params,
                    body,
                    env,
                })])
            }
            // Only a transaction runs an action; a table literal is data
            Expr::Action(_) | Expr::Table { .. } => Functions::new(),
            Expr::Call { func, args } => {
                let callee = self.visit(func, service, locals, reads);
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.visit(arg, service, locals, reads))
                    .collect();
                let mut result = Functions::new();
                for function in callee {
                    result.extend(self.apply(function, &args, reads));
                }
                result
            }
            Expr::Select {
                table_name,
                where_clause,
                ..
            } => {
                self.visit(where_clause, service, locals, reads);
                if !locals.iter().any(|(local, _)| local == table_name) {
                    reads.direct.insert((service, *table_name));
                }
                Functions::new()
            }
            // Every other form reads what its parts read and may be any
            // function one of them may be
            _ => {
                let mut value = Functions::new();
                for child in expr.children() {
                    value.extend(self.visit(child, service, locals, reads));
                }
                value
            }
        }
    }

    fn literal(&mut self, val: &'a Value) -> Functions {
        match val {
            Value::Closure {
                params,
                body,
                env,
                service_name,
                ..
            } => {
                let env = env
                    .iter()
                    .map(|(name, value)| (*name, self.literal(value)))
                    .collect();
                BTreeSet::from([self.function(Lambda {
                    service: *service_name,
                    params,
                    body,
                    env,
                })])
            }
            _ => Functions::new(),
        }
    }

    /// The function that is `lambda`
    fn function(&mut self, lambda: Lambda<'a>) -> Function {
        let identity = (
            lambda.service,
            lambda.body as *const Expr,
            lambda.env.clone(),
        );
        let lambdas = &mut self.lambdas;
        *self.functions.entry(identity).or_insert_with(|| {
            lambdas.push(lambda);
            lambdas.len() - 1
        })
    }

    /// Record a read of `member`, and return the functions it may hold
    fn read(&mut self, member: Member, reads: &mut Reads) -> Functions {
        reads.direct.insert(member);
        if let Some(functions) = self.values.get(&member) {
            return functions.clone();
        }
        if let Some(versions) = self.published.get(&member).cloned() {
            let mut functions = Functions::new();
            for value in versions {
                functions.extend(self.literal(value));
            }
            return functions;
        }
        let code = match self.decl(member) {
            Some(Decl::VarDecl { val, .. } | Decl::DefDecl { val, .. }) => val,
            Some(Decl::TableDecl { .. }) => return Functions::new(),
            // A member of a service known here only by name may hold any
            None => return BTreeSet::from([UNKNOWN]),
        };
        if !self.computing.insert(member) {
            return BTreeSet::from([UNKNOWN]);
        }
        // What the member's own code reads belongs to that member, not here,
        // and what it holds does not depend on the applications enclosing this
        let applying = std::mem::take(&mut self.applying);
        let functions = self.visit(code, member.0, &[], &mut Reads::default());
        self.applying = applying;
        self.computing.remove(&member);
        self.values.insert(member, functions.clone());
        functions
    }

    /// Add to `reads` what applying `function` to `args` reads, and return the
    /// functions its result may be
    fn apply(&mut self, function: Function, args: &[Functions], reads: &mut Reads) -> Functions {
        let Some(Lambda {
            service,
            params,
            body,
            env,
        }) = self.lambdas.get(function).cloned()
        else {
            return BTreeSet::from([UNKNOWN]);
        };
        let application = (function, args.to_vec());
        if let Some((more, result)) = self.applied.get(&application) {
            reads.activated.extend(more);
            return result.clone();
        }
        // A recursive application reads what the application it reenters
        // reads. One past the depth limit is cut off like one reentering the
        // outermost application, which no other analysis would follow deeper
        let depth = self.applying.len();
        let reentered = self
            .applying
            .iter()
            .position(|(other, _)| *other == application);
        if reentered.is_some() || depth >= MAX_SCOPE_DEPTH {
            if let Some((_, outermost)) = self.applying.last_mut() {
                *outermost = reentered.unwrap_or(0).min(*outermost);
            }
            return BTreeSet::from([UNKNOWN]);
        }
        let mut locals = env;
        locals.extend(
            params
                .iter()
                .map(|param| param.name)
                .zip(args.iter().cloned()),
        );
        self.applying.push((application, depth));
        let mut own = Reads::default();
        let result = self.visit(body, service, &locals, &mut own);
        let (application, outermost) = self.applying.pop().expect("pushed above");
        let own: HashSet<Member> = own.all().collect();
        reads.activated.extend(&own);
        match self.applying.last_mut() {
            // Its reads lack those of the enclosing application it reentered
            Some((_, enclosing)) if outermost < depth => *enclosing = outermost.min(*enclosing),
            _ => {
                self.applied.insert(application, (own, result.clone()));
            }
        }
        result
    }
}

/// A program whose declarations cannot be initialized in order, or whose
/// direct dependency graph has a cycle
#[derive(Debug)]
pub enum Error {
    /// Initializing this member would read a member declared after it
    ForwardReference(Symbol),
    /// This def reads itself, directly or through other defs
    Cycle { service: Symbol, member: Symbol },
}

/// A member of a program, in initialization order: services in program
/// order, their members in declaration order
struct Declared {
    member: Member,
    /// The position of its service in the program, and its index in the service
    at: (usize, usize),
    def: bool,
    /// What its code reads; None for a table
    reads: Option<Reads>,
}

/// Check that `program` can be initialized in declaration order and that its
/// direct dependency graph is acyclic, from one analysis of its declarations
pub(crate) fn check(program: &[Stmt]) -> Result<(), Error> {
    let mut effects = Effects::new(program, HashMap::new());
    let mut services = Vec::new();
    for stmt in program {
        if let Stmt::Service { name, .. } = stmt {
            if !services.contains(name) {
                services.push(*name);
            }
        }
    }
    let mut declared = Vec::new();
    for (position, &service) in services.iter().enumerate() {
        let Some(decls) = effects.services.get(&service).copied() else {
            continue;
        };
        for (index, decl) in decls.iter().enumerate() {
            let (Decl::VarDecl { name, .. }
            | Decl::DefDecl { name, .. }
            | Decl::TableDecl { name, .. }) = decl;
            declared.push(Declared {
                member: (service, *name),
                at: (position, index),
                def: matches!(decl, Decl::DefDecl { .. }),
                reads: effects.member_reads(service, decl).map(|(_, reads)| reads),
            });
        }
    }
    if let Some(member) = forward_reference(&declared) {
        return Err(Error::ForwardReference(member));
    }
    match dependency_cycle(&declared) {
        Some((service, member)) => Err(Error::Cycle { service, member }),
        None => Ok(()),
    }
}

/// The first member whose initialization reads a member not yet initialized:
/// one of its own service declared after it, or, through a function it
/// applies, one of a service declared after its own. Of several such reads,
/// the one declared first
fn forward_reference(declared: &[Declared]) -> Option<Symbol> {
    let position: HashMap<Symbol, usize> = declared.iter().map(|d| (d.member.0, d.at.0)).collect();
    let index: HashMap<Member, usize> = declared.iter().map(|d| (d.member, d.at.1)).collect();
    for Declared {
        member: (service, _),
        at: (p, i),
        reads,
        ..
    } in declared
    {
        let Some(reads) = reads else {
            continue;
        };
        let later = |read: &Member| match read.0 == *service {
            true => index.get(read).is_none_or(|j| j >= i),
            false => reads.activated.contains(read) && position.get(&read.0).is_some_and(|q| q > p),
        };
        if let Some((_, member)) = reads
            .all()
            .filter(later)
            .min_by_key(|read| (position[&read.0], index.get(read)))
        {
            return Some(member);
        }
    }
    None
}

/// The first declared def that reads itself, directly or through other defs:
/// the direct dependency graph of a program must be acyclic (§2.1.4). Vars
/// are sources, so a cycle runs through defs only
fn dependency_cycle(declared: &[Declared]) -> Option<Member> {
    let mut graph = DiGraphMap::<Member, ()>::new();
    for d in declared.iter().filter(|d| d.def) {
        graph.add_node(d.member);
        for input in d.reads.iter().flat_map(Reads::all) {
            graph.add_edge(input, d.member, ());
        }
    }
    let cyclic: HashSet<Member> = petgraph::algo::tarjan_scc(&graph)
        .into_iter()
        .filter(|component| component.len() > 1 || graph.contains_edge(component[0], component[0]))
        .flatten()
        .collect();
    declared
        .iter()
        .find(|d| d.def && cyclic.contains(&d.member))
        .map(|d| d.member)
}
