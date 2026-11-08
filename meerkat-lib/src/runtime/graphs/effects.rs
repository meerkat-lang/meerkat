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
//! The functions a member may hold come from its declaration: a var holds
//! what its initializer may evaluate to (an action may store another function
//! in it only while every def applying the var keeps the reads found here,
//! see `Manager::check_function_writes`), and a remote member what its owner
//! published to the defs here that read it. Applying a function whose code is
//! not known here reads nothing; the runtime analyzes the code when it
//! arrives, before any def can apply it
//!
//! A recursive application returns what the application it reenters is
//! assumed to return, at first nothing; when the analysis of that application
//! returns more, it runs again under the larger assumption, until its result
//! stands still. The functions of a program are finite, so it does: lambdas
//! are told apart by their code and the functions they captured, and a lambda
//! capturing, in some function, a lambda of its own code point (a recursion
//! wrapping a closure around the one before) is read as its code point's
//! summary, one lambda capturing what every such lambda captured
//!
//! The analysis is bounded all the same: recursion `MAX_ANALYSIS_DEPTH` deep,
//! counting nested expressions, applications and captured closures, or a
//! result that does not stand still within
//! `MAX_ANALYSIS_ROUNDS` rounds, exceeds the depth limit, which ends that
//! member's analysis at once. Its reads are then reported incomplete rather
//! than used: `check` rejects such a program, and the runtime refuses a
//! function value that would leave a def's reads incomplete

use super::free_var::free_var;
use super::{cyclic_members, ServiceGraphs};
use crate::ast::{Decl, Expr, Stmt, Value};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::limits::{MAX_ANALYSIS_DEPTH, MAX_ANALYSIS_ROUNDS};
use crate::runtime::tt::Param;
use petgraph::graphmap::DiGraphMap;
use std::collections::{BTreeSet, HashMap, HashSet};

/// A member, as `(service, member)`
pub(crate) type Member = (Symbol, Symbol);

/// The error text for a member whose analysis exceeded the depth limit
pub(crate) fn depth_limit_message(member: &str) -> String {
    format!("depth limit exceeded in the dependency analysis of '{member}'")
}

/// A function a value may be when it is applied: the lambda at this index of
/// `Effects::lambdas`, or `UNKNOWN`
type Function = usize;

/// A function whose code is not known here
const UNKNOWN: Function = usize::MAX;

/// The functions a value may be; empty for a value that is not a function
type Functions = BTreeSet<Function>;

/// The current member could not be analyzed within the depth limit
#[derive(Clone, Copy)]
struct DepthLimit;

/// A code point: a lambda expression of a service
type Point = (Symbol, *const Expr);

/// The service and member positions in initialization order.
type Position = (usize, usize);

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

/// A function applied to the functions each argument may be, at initialization
/// or with all members present. Captures can differ between these contexts.
type Application = (Function, Vec<Functions>, Option<Position>);

/// An application being analyzed
struct Applying {
    application: Application,
    /// The depth of the outermost application it reentered, directly or
    /// through the applications it made; its own depth if none
    outermost: usize,
    /// The functions its result is assumed to be where it reenters itself
    assumed: Functions,
    /// Whether it reentered itself in the current round
    reentered: bool,
}

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
    /// Each member's position and declaration: the first of its name in its service
    decls: HashMap<Member, (Position, &'a Decl)>,
    /// A declaration's initializer sees only members before this position;
    /// ordinary reactive evaluation sees the fully initialized services.
    initializing: Option<Position>,
    /// Capturing a local named like a member can differ during initialization.
    shadowing: bool,
    /// Functions members hold besides those their declarations evaluate to,
    /// analyzed with them: the versions of its code a remote owner delivered
    /// (while a new one is on its way to some readers, a member may hold any
    /// version they hold), the values local members hold, and the functions
    /// a write would store
    published: HashMap<Member, Vec<&'a Value>>,
    /// Each lambda met, at the index that is its `Function`. Lambdas are the
    /// same function when they are the same code with the same captured locals
    lambdas: Vec<Lambda<'a>>,
    functions: HashMap<(Symbol, *const Expr, Locals), Function>,
    /// Summaries preserve which names are captured: a missing capture must
    /// remain a service-member read, even when a nested lambda captured it.
    summaries: HashMap<(Point, Vec<Symbol>, Option<Position>), Function>,
    /// The depth of the recursion: the expressions being visited and the
    /// applications being analyzed, each within the one before
    depth: usize,
    /// Each member's initial or reactive functions and whether captures depend
    /// on initialization order, or failure to analyze them within the limit.
    values: HashMap<(Member, bool), (Result<Functions, DepthLimit>, bool)>,
    /// Members whose value is being computed
    computing: HashSet<(Member, bool)>,
    /// Applications being analyzed, innermost last
    applying: Vec<Applying>,
    /// The reads, result and capture sensitivity of each application whose
    /// analysis did not reenter an application enclosing it
    applied: HashMap<Application, (HashSet<Member>, Functions, bool)>,
}

impl<'a> Effects<'a> {
    pub(crate) fn new(program: &'a [Stmt], published: HashMap<Member, Vec<&'a Value>>) -> Self {
        let services: HashMap<Symbol, &'a [Decl]> = super::analysis::service_declarations(program)
            .into_iter()
            .filter_map(|(name, stmt)| match stmt {
                Stmt::Service { decls, .. } => Some((name, decls.as_slice())),
                _ => None,
            })
            .collect();
        let mut decls = HashMap::new();
        for (position, stmt) in program.iter().enumerate() {
            let Stmt::Service { name, .. } = stmt else {
                continue;
            };
            for (index, decl) in services[name].iter().enumerate() {
                decls
                    .entry((*name, decl.name()))
                    .or_insert(((position, index), decl));
            }
        }
        Effects {
            services,
            decls,
            published,
            ..Default::default()
        }
    }

    /// The dependency graphs of `decls`, declarations of `service`: an edge or
    /// cross-service dependency for each member a def's code reads. A var or a
    /// table is a source: what a var's initializer reads, the transaction
    /// initializing it reads once, and is no input of its value afterwards
    pub(crate) fn graphs(&mut self, service: Symbol, decls: &'a [Decl]) -> ServiceGraphs {
        let mut graphs = ServiceGraphs::new();
        for decl in decls {
            let name = decl.name();
            match decl {
                Decl::VarDecl { .. } => graphs.vars.insert(name),
                Decl::DefDecl { .. } => graphs.defs.insert(name),
                Decl::TableDecl { .. } => graphs.tables.insert(name),
            };
            graphs.reactive_graph.add_node(name);
        }
        for decl in decls {
            let Decl::DefDecl { .. } = decl else {
                continue;
            };
            let Some((name, reads, complete)) = self.member_reads(service, decl, false) else {
                continue;
            };
            if !complete {
                graphs.incomplete.insert(name);
            }
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

    /// What evaluating the code of `decl`, a declaration of `service`, reads,
    /// and whether the analysis found all of it within the depth limit
    fn member_reads(
        &mut self,
        service: Symbol,
        decl: &'a Decl,
        initializing: bool,
    ) -> Option<(Symbol, Reads, bool)> {
        if matches!(decl, Decl::TableDecl { .. }) {
            return None;
        }
        self.shadowing = false;
        let mut reads = Reads::default();
        let complete = self
            .member_value(service, decl, initializing, &mut reads)
            .is_ok();
        Some((decl.name(), reads, complete))
    }

    fn decl(&self, member: Member) -> Option<&'a Decl> {
        self.decls.get(&member).map(|(_, decl)| *decl)
    }

    /// A var keeps its initialized value. A def can be evaluated both during
    /// initialization and later, when all its service's members exist.
    fn member_value(
        &mut self,
        service: Symbol,
        decl: &'a Decl,
        initializing: bool,
        reads: &mut Reads,
    ) -> Result<Functions, DepthLimit> {
        let (Decl::VarDecl { val, .. } | Decl::DefDecl { val, .. }) = decl else {
            return Ok(Functions::new());
        };
        let position = self.decls[&(service, decl.name())].0;
        let previous = self.initializing;
        self.initializing = initializing.then_some(position);
        let result = self.visit(val, service, &[], reads);
        // Usually these contexts are identical. Only a local shadowing a
        // service member can make the initial captures differ from later ones.
        let result = if !initializing && self.shadowing {
            self.initializing = Some(position);
            result.and_then(|mut functions| {
                functions.extend(self.visit(val, service, &[], reads)?);
                Ok(functions)
            })
        } else {
            result
        };
        self.initializing = previous;
        result
    }

    /// Add to `reads` what evaluating `expr` in `service` reads, and return the
    /// functions its value may be
    fn visit(
        &mut self,
        expr: &'a Expr,
        service: Symbol,
        locals: &[(Symbol, Functions)],
        reads: &mut Reads,
    ) -> Result<Functions, DepthLimit> {
        self.descend()?;
        let result = self.visit_expr(expr, service, locals, reads);
        self.depth -= 1;
        result
    }

    /// One level deeper, unless the recursion is at its limit
    fn descend(&mut self) -> Result<(), DepthLimit> {
        if self.depth >= MAX_ANALYSIS_DEPTH {
            return Err(DepthLimit);
        }
        self.depth += 1;
        Ok(())
    }

    fn visit_expr(
        &mut self,
        expr: &'a Expr,
        service: Symbol,
        locals: &[(Symbol, Functions)],
        reads: &mut Reads,
    ) -> Result<Functions, DepthLimit> {
        Ok(match expr {
            Expr::Literal { val } => self.literal(val)?,
            // A name that is not a local is a member of the code's service,
            // declared here or not
            Expr::Variable { name } => match locals.iter().rev().find(|(local, _)| local == name) {
                Some((_, functions)) => functions.clone(),
                None => self.read((service, *name), reads)?,
            },
            &Expr::MemberAccess {
                service_name,
                member_name,
            } => self.read((service_name, member_name), reads)?,
            Expr::Func { params, body, .. } => {
                // As in `eval`, a closure captures the locals free in its body,
                // except one named like a member that already exists.
                let bound = params.iter().map(|param| param.name).collect();
                let free = free_var(body, &bound);
                let env = locals
                    .iter()
                    .filter(|(local, _)| {
                        if !free.contains(local) {
                            return false;
                        }
                        let Some(&(position, _)) = self.decls.get(&(service, *local)) else {
                            return true;
                        };
                        self.shadowing = true;
                        self.initializing.is_some_and(|at| position >= at)
                    })
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
                let callee = self.visit(func, service, locals, reads)?;
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.visit(arg, service, locals, reads))
                    .collect::<Result<_, _>>()?;
                let mut result = Functions::new();
                for function in callee {
                    result.extend(self.apply(function, &args, reads)?);
                }
                result
            }
            Expr::Select {
                table_name,
                where_clause,
                ..
            } => {
                let result = self.visit(where_clause, service, locals, reads);
                if !locals.iter().any(|(local, _)| local == table_name) {
                    reads.direct.insert((service, *table_name));
                }
                result?;
                Functions::new()
            }
            // Every other form reads what its parts read and may be any
            // function one of them may be
            _ => {
                let mut value = Functions::new();
                for child in expr.children() {
                    value.extend(self.visit(child, service, locals, reads)?);
                }
                value
            }
        })
    }

    fn literal(&mut self, val: &'a Value) -> Result<Functions, DepthLimit> {
        match val {
            Value::Closure {
                params,
                body,
                env,
                service_name,
                ..
            } => {
                self.descend()?;
                let result = env
                    .iter()
                    .map(|(name, value)| self.literal(value).map(|functions| (*name, functions)))
                    .collect::<Result<_, _>>()
                    .map(|env| {
                        BTreeSet::from([self.function(Lambda {
                            service: *service_name,
                            params,
                            body,
                            env,
                        })])
                    });
                self.depth -= 1;
                result
            }
            _ => Ok(Functions::new()),
        }
    }

    /// The function that is `lambda`: the same code with the same captured
    /// locals is the same function. A lambda that captured, in some function
    /// a local may be, a lambda of its own code point is one of a family a
    /// recursion builds without end, each around the one before; it is the
    /// point's summary, one function capturing what every lambda of the
    /// family captured, so the functions of a program stay finite
    fn function(&mut self, lambda: Lambda<'a>) -> Function {
        let point = (lambda.service, lambda.body as *const Expr);
        if self.nests(point, &lambda.env) {
            return self.summarize(point, lambda);
        }
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

    /// Whether a function some local of `env` may be is a lambda of `point`
    /// with the same captured names, itself or through what it captured.
    fn nests(&self, point: Point, env: &Locals) -> bool {
        let mut seen = HashSet::new();
        let mut pending: Vec<Function> = env.iter().flat_map(|(_, f)| f.iter().copied()).collect();
        while let Some(function) = pending.pop() {
            let Some(lambda) = self.lambdas.get(function) else {
                continue;
            };
            if !seen.insert(function) {
                continue;
            }
            if (lambda.service, lambda.body as *const Expr) == point
                && lambda
                    .env
                    .iter()
                    .map(|(name, _)| name)
                    .eq(env.iter().map(|(name, _)| name))
            {
                return true;
            }
            pending.extend(lambda.env.iter().flat_map(|(_, f)| f.iter().copied()));
        }
        false
    }

    /// The summary of `point`, capturing what `lambda` captured as well: a
    /// lambda of the point with the same captured names is read as the
    /// summary, and what it captured joins. The applications analyzed with
    /// a smaller summary are forgotten, to be analyzed again with this one
    fn summarize(&mut self, point: Point, lambda: Lambda<'a>) -> Function {
        let names: Vec<_> = lambda.env.iter().map(|(name, _)| *name).collect();
        let summary = *self
            .summaries
            .entry((
                point,
                names.clone(),
                self.initializing.filter(|_| self.shadowing),
            ))
            .or_insert_with(|| {
                self.lambdas.push(Lambda {
                    env: Vec::new(),
                    ..lambda.clone()
                });
                self.lambdas.len() - 1
            });
        let mut grew = false;
        let mut folded = HashSet::from([summary]);
        let mut pending = vec![lambda.env];
        while let Some(env) = pending.pop() {
            for (name, functions) in env {
                for function in functions {
                    let of_point = self.lambdas.get(function).is_some_and(|l| {
                        (l.service, l.body as *const Expr) == point
                            && l.env.iter().map(|(name, _)| name).eq(names.iter())
                    });
                    if of_point && folded.insert(function) {
                        pending.push(self.lambdas[function].env.clone());
                    }
                    let captured = if of_point { summary } else { function };
                    let env = &mut self.lambdas[summary].env;
                    match env.iter_mut().find(|(local, _)| *local == name) {
                        Some((_, set)) => grew |= set.insert(captured),
                        None => {
                            env.push((name, BTreeSet::from([captured])));
                            grew = true;
                        }
                    }
                }
            }
        }
        if grew {
            self.applied.clear();
        }
        summary
    }

    /// Record a read of `member`, and return the functions it may hold
    fn read(&mut self, member: Member, reads: &mut Reads) -> Result<Functions, DepthLimit> {
        reads.direct.insert(member);
        let decl = self.decl(member);
        let initializing =
            self.initializing.is_some() || matches!(decl, Some(Decl::VarDecl { .. }));
        let key = (member, initializing);
        if let Some((functions, shadowing)) = self.values.get(&key) {
            self.shadowing |= shadowing;
            return functions.clone();
        }
        let mut published = Functions::new();
        for value in self.published.get(&member).cloned().into_iter().flatten() {
            published.extend(self.literal(value)?);
        }
        let decl = match decl {
            Some(Decl::TableDecl { .. }) => return Ok(Functions::new()),
            Some(decl) => decl,
            // A member of a service known here only by name may hold any
            // function, unless its owner published what it holds
            None if published.is_empty() => return Ok(BTreeSet::from([UNKNOWN])),
            None => {
                self.values.insert(key, (Ok(published.clone()), false));
                return Ok(published);
            }
        };
        if !self.computing.insert(key) {
            return Ok(BTreeSet::from([UNKNOWN]));
        }
        // What the member's own code reads belongs to that member, not here,
        // and what it holds does not depend on the applications enclosing this
        let applying = std::mem::take(&mut self.applying);
        let outer_shadowing = std::mem::take(&mut self.shadowing);
        let functions = self
            .member_value(member.0, decl, initializing, &mut Reads::default())
            .map(|mut declared| {
                declared.append(&mut published);
                declared
            });
        self.applying = applying;
        self.computing.remove(&key);
        self.values.insert(key, (functions.clone(), self.shadowing));
        self.shadowing |= outer_shadowing;
        functions
    }

    /// Add to `reads` what applying `function` to `args` reads, and return the
    /// functions its result may be
    fn apply(
        &mut self,
        function: Function,
        args: &[Functions],
        reads: &mut Reads,
    ) -> Result<Functions, DepthLimit> {
        let Some(Lambda {
            service,
            params,
            body,
            ..
        }) = self.lambdas.get(function).cloned()
        else {
            return Ok(BTreeSet::from([UNKNOWN]));
        };
        let application = (function, args.to_vec(), self.initializing);
        let cached = self.applied.get(&application).or_else(|| {
            // Calls whose captures do not depend on initialization order keep
            // sharing one result across all declarations, as before.
            let shared = (function, args.to_vec(), None);
            self.applied
                .get(&shared)
                .filter(|(_, _, shadowing)| !shadowing)
        });
        if let Some((more, result, shadowing)) = cached {
            reads.activated.extend(more);
            self.shadowing |= shadowing;
            return Ok(result.clone());
        }
        // A recursive application returns what the application it reenters
        // is assumed to; the applications in between are not cached
        let depth = self.applying.len();
        if let Some(position) = self
            .applying
            .iter()
            .position(|other| other.application == application)
        {
            self.applying[position].reentered = true;
            if let Some(innermost) = self.applying.last_mut() {
                innermost.outermost = position.min(innermost.outermost);
            }
            return Ok(self.applying[position].assumed.clone());
        }
        // Propagation of this failure stops the member's analysis and leaves
        // every application enclosing it out of the cache
        self.descend()?;
        self.applying.push(Applying {
            application,
            outermost: depth,
            assumed: Functions::new(),
            reentered: false,
        });
        let outer_shadowing = std::mem::take(&mut self.shadowing);
        let mut own = Reads::default();
        let mut rounds = 0;
        let result = loop {
            // The function may be a summary that grows while its body is
            // analyzed: the locals are what it captures as this round starts
            let env = self.lambdas[function].env.clone();
            let mut locals = env.clone();
            locals.extend(
                params
                    .iter()
                    .map(|param| param.name)
                    .zip(args.iter().cloned()),
            );
            let result = self.visit(body, service, &locals, &mut own);
            let widened = self.lambdas[function].env != env;
            let current = self.applying.last_mut().expect("pushed above");
            match result {
                // Reentered under too small an assumption, or the function
                // came to capture more: what it returns must include what it
                // returns here. Each round adds to the assumption, the
                // captured functions or the reads, so the rounds end
                Ok(result)
                    if widened || (current.reentered && !result.is_subset(&current.assumed)) =>
                {
                    rounds += 1;
                    if rounds >= MAX_ANALYSIS_ROUNDS {
                        break Err(DepthLimit);
                    }
                    current.assumed.extend(result);
                    current.reentered = false;
                    current.outermost = depth;
                }
                result => break result,
            }
        };
        self.depth -= 1;
        let Applying {
            mut application,
            outermost,
            ..
        } = self.applying.pop().expect("pushed above");
        let own: HashSet<Member> = own.all().collect();
        reads.activated.extend(&own);
        let shadowing = self.shadowing;
        self.shadowing |= outer_shadowing;
        let result = result?;
        match self.applying.last_mut() {
            // Its reads lack those of the enclosing application it reentered
            Some(enclosing) if outermost < depth => {
                enclosing.outermost = outermost.min(enclosing.outermost)
            }
            _ => {
                if !shadowing {
                    application.2 = None;
                }
                self.applied
                    .insert(application, (own, result.clone(), shadowing));
            }
        }
        Ok(result)
    }
}

/// A program whose declarations cannot be initialized in order, whose direct
/// dependency graph has a cycle, or whose analysis exceeds the depth limit
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Initializing this member would read a member declared after it
    ForwardReference(Symbol),
    /// This def reads itself, directly or through other defs
    Cycle { service: Symbol, member: Symbol },
    /// The analysis of what this member reads recursed `MAX_ANALYSIS_DEPTH`
    /// deep, or a recursive application's result did not stand still within
    /// `MAX_ANALYSIS_ROUNDS` rounds
    DepthLimitExceeded { service: Symbol, member: Symbol },
}

impl Error {
    /// The error in program terms
    pub(crate) fn message(&self, interner: &Interner) -> String {
        match self {
            Error::Cycle { service, member } => format!(
                "dependency cycle detected at service '{}', member '{}'",
                interner.get(*service),
                interner.get(*member)
            ),
            Error::ForwardReference(member) => format!(
                "illegal eager forward reference or dependency cycle on '{}'",
                interner.get(*member)
            ),
            Error::DepthLimitExceeded { service, member } => depth_limit_message(&format!(
                "{}.{}",
                interner.get(*service),
                interner.get(*member)
            )),
        }
    }
}

/// A member of a program, in initialization order: services in program
/// order, their members in declaration order
struct Declared {
    member: Member,
    /// The position of its service in the program, and its index in the service
    at: (usize, usize),
    def: bool,
    /// Initial captures can differ from the ones made by later evaluations.
    shadowing: bool,
    /// What its code reads; None for a table
    reads: Option<Reads>,
}

/// Check that `program` can be initialized in declaration order, that its
/// direct dependency graph is acyclic, and that the analysis of each member
/// stays within the depth limit, from one analysis of its declarations
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
            let name = decl.name();
            let reads = match effects.member_reads(service, decl, true) {
                Some((_, _, false)) => {
                    return Err(Error::DepthLimitExceeded {
                        service,
                        member: name,
                    })
                }
                Some((_, reads, true)) => Some(reads),
                None => None,
            };
            declared.push(Declared {
                member: (service, name),
                at: (position, index),
                def: matches!(decl, Decl::DefDecl { .. }),
                shadowing: effects.shadowing,
                reads,
            });
        }
    }
    if let Some(member) = forward_reference(&declared) {
        return Err(Error::ForwardReference(member));
    }
    for member in declared
        .iter_mut()
        .filter(|member| member.def && member.shadowing)
    {
        let decl = effects.decl(member.member).expect("collected above");
        let Some((name, reads, complete)) = effects.member_reads(member.member.0, decl, false)
        else {
            continue;
        };
        if !complete {
            return Err(Error::DepthLimitExceeded {
                service: member.member.0,
                member: name,
            });
        }
        member.reads = Some(reads);
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
    let cyclic: HashSet<Member> = cyclic_members(&graph).collect();
    declared
        .iter()
        .find(|d| d.def && cyclic.contains(&d.member))
        .map(|d| d.member)
}
