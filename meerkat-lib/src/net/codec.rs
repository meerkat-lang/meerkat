//! Network codec for `AST` elements
//!
//! Provides encoding and decoding functions to map between the native
//! `AST` types and the serialized network representation variants

use crate::error::{Error, Result};
use crate::net::ast::{
    NetActionStmt, NetBinOp, NetExpr, NetField, NetParam, NetServiceType, NetTableType, NetType,
    NetUnOp, NetValue,
};
use crate::net::types::{LockGroup, MeerkatMessage};
use crate::runtime::ast::{ActionStmt, BinOp, Expr, Field, TableType, UnOp, Value};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::limits::{
    MAX_IDENTIFIER_LENGTH, MAX_NET_REQUEST_SOURCE_LENGTH, MAX_NET_REQUEST_STRING_LENGTH,
    MAX_STRING_LITERAL_LENGTH, MAX_TYPE_DEPTH,
};
use crate::runtime::tt::{Param, ServiceType, TupleType, Type};
use std::collections::HashMap;

fn validate_identifier(s: &str) -> Result<()> {
    if s.len() > MAX_IDENTIFIER_LENGTH {
        return Err(Error::LimitExceeded(format!(
            "identifier exceeds maximum length of {} characters",
            MAX_IDENTIFIER_LENGTH
        )));
    }
    // Match the lexer's identifier rule ([A-Za-z_][A-Za-z0-9_]*) so a name
    // arriving over the wire is rejected unless the parser would also accept
    // it locally. Without this, length-valid but non-identifier strings
    // (empty, whitespace, punctuation) could still be interned from the wire.
    let mut chars = s.chars();
    let valid = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    };
    if !valid {
        return Err(Error::LimitExceeded(format!(
            "invalid identifier (must match [A-Za-z_][A-Za-z0-9_]*): {:?}",
            s
        )));
    }
    Ok(())
}

fn validate_string_literal(s: &str) -> Result<()> {
    if s.len() > MAX_STRING_LITERAL_LENGTH {
        return Err(Error::LimitExceeded(format!(
            "string literal exceeds maximum length of {} characters",
            MAX_STRING_LITERAL_LENGTH
        )));
    }
    Ok(())
}

/// #39: validate the length-bounded string fields of a `ServiceCodeRequest`
/// arriving over the wire. `path` and `reply_to` are a resource path and a
/// network address, not identifiers, so they are length-checked but not
/// charset-validated or interned. The source in the response is unbounded.
pub fn validate_service_code_request(path: &str, reply_to: &str) -> Result<()> {
    for (field, value) in [("path", path), ("reply_to", reply_to)] {
        if value.len() > MAX_NET_REQUEST_STRING_LENGTH {
            return Err(Error::LimitExceeded(format!(
                "{} exceeds maximum length of {} bytes",
                field, MAX_NET_REQUEST_STRING_LENGTH
            )));
        }
    }
    Ok(())
}

/// Validate incoming `ServiceCodeResponse` source and path, returning the
/// validated service name
///
/// Args:
///     `path` (`&str`): Requested file path
///     `source` (`&str`): Received source code
///
/// Returns:
///     `Result<String>`: Validated service name
///
/// Raises:
///     `Error::LimitExceeded`: If `source` length exceeds limit or `path` is invalid
pub fn decode_source_response(path: &str, source: &str) -> Result<String> {
    if source.len() > MAX_NET_REQUEST_SOURCE_LENGTH {
        return Err(Error::LimitExceeded(format!(
            "source size exceeds maximum length of {} bytes",
            MAX_NET_REQUEST_SOURCE_LENGTH
        )));
    }
    let service_name = path.strip_suffix(".mkt").unwrap_or(path);
    validate_identifier(service_name)?;
    Ok(service_name.to_string())
}

/// Validate and decode an `UpdateServiceRequest` arriving over the wire
///
/// Enforces zero-trust identifier and payload size limits
///
/// Args:
///     `service_name` (`&str`): Name of the service to update
///     `source` (`&str`): Source code payload
///     `interner` (`&mut Interner`): Interner for symbol creation
///
/// Returns:
///     `Result<(Symbol, String)>`: Validated interned service symbol
///     and source string
///
/// Raises:
///     `Error::LimitExceeded`: If identifier is invalid or source exceeds
///     max length
pub fn decode_update_service_request(
    service_name: &str,
    source: &str,
    interner: &mut Interner,
) -> Result<(Symbol, String)> {
    validate_identifier(service_name)?;
    if source.len() > MAX_NET_REQUEST_SOURCE_LENGTH {
        return Err(Error::LimitExceeded(format!(
            "source size exceeds maximum length of {} bytes",
            MAX_NET_REQUEST_SOURCE_LENGTH
        )));
    }
    Ok((interner.insert(service_name), source.to_string()))
}

/// Validate identifier fields of a `LookupRequest` message arriving over
/// the wire
///
/// Args:
///     service (&str): Service identifier
///     member (&str): Member identifier
///
/// Returns:
///     Result<()>: Ok if valid, or an error if invalid
///
/// Raises:
///     Error::LimitExceeded: If any identifier is invalid or exceeds max length
pub fn validate_lookup_request(service: &str, member: &str) -> Result<()> {
    validate_identifier(service)?;
    validate_identifier(member)?;
    Ok(())
}

/// Validate identifier fields of an `ActionRequest` message arriving over
/// the wire
///
/// Args:
///     service (&str): Service identifier
///
/// Returns:
///     Result<()>: Ok if valid, or an error if invalid
///
/// Raises:
///     Error::LimitExceeded: If identifier is invalid or exceeds max length
pub fn validate_action_request(service: &str) -> Result<()> {
    validate_identifier(service)
}

/// Validate identifier fields of a `LockRequest` message arriving over
/// the wire
///
/// Args:
///     services (&HashMap<String, LockGroup>): Map of service names to lock groups
///
/// Returns:
///     Result<()>: Ok if valid, or an error if invalid
///
/// Raises:
///     Error::LimitExceeded: If any service or member identifier is invalid or exceeds max length
pub fn validate_lock_request(services: &HashMap<String, LockGroup>) -> Result<()> {
    for (svc, group) in services {
        validate_identifier(svc)?;
        for r in &group.reads {
            validate_identifier(r)?;
        }
        for w in &group.writes {
            validate_identifier(w)?;
        }
    }
    Ok(())
}

/// #39: Handle a `ServiceCodeRequest` end-to-end: validate the length-bounded
/// fields, safely resolve the requested path against the server's base
/// directory, read that file, and return a `ServiceCodeResponse` with its
/// source, or a `ServiceCodeError` describing any failure.
///
/// Shared by `run_server` and the integration tests so both exercise the same
/// logic. The whole file is returned so the client gets any imports and other
/// services the requested file needs.
pub fn serve_service_code(
    request_id: u64,
    path: String,
    reply_to: &str,
    base_dir: &std::path::Path,
) -> MeerkatMessage {
    let result = validate_service_code_request(&path, reply_to)
        .and_then(|()| resolve_served_file(base_dir, &path))
        .and_then(|file| {
            std::fs::read_to_string(&file)
                .map_err(|e| Error::LimitExceeded(format!("failed to read requested file: {}", e)))
        });
    match result {
        Ok(source) => MeerkatMessage::ServiceCodeResponse {
            request_id,
            path,
            source,
        },
        Err(e) => MeerkatMessage::ServiceCodeError {
            request_id,
            error: e.to_string(),
        },
    }
}

/// #39: Safely resolve a client-requested file path against a server base
/// directory, for serving `.mkt` source over the network.
///
/// This is a zero-trust boundary: the path comes from the network, so it is
/// validated to prevent directory traversal. An absolute path is rejected, and
/// the resolved path is canonicalized and required to stay within the
/// canonicalized base directory (blocking `..` and symlink escapes). Returns
/// the validated path to read, or an error describing why it was rejected.
pub fn resolve_served_file(
    base_dir: &std::path::Path,
    requested: &str,
) -> Result<std::path::PathBuf> {
    use std::path::Path;

    // Reject absolute paths outright; served files are always relative to base.
    if Path::new(requested).is_absolute() {
        return Err(Error::LimitExceeded(format!(
            "requested path must be relative, got absolute: {:?}",
            requested
        )));
    }

    let joined = base_dir.join(requested);

    // Canonicalize both so `..` and symlinks are resolved, then require the
    // target to remain within the base directory.
    let canon_base = base_dir.canonicalize().map_err(|e| {
        Error::LimitExceeded(format!("server base directory is unavailable: {}", e))
    })?;
    let canon_target = joined
        .canonicalize()
        .map_err(|_| Error::LimitExceeded(format!("requested file not found: {:?}", requested)))?;
    if !canon_target.starts_with(&canon_base) {
        return Err(Error::LimitExceeded(format!(
            "requested path escapes the served directory: {:?}",
            requested
        )));
    }
    Ok(canon_target)
}

/// #24: validate and intern the identifier fields of a `RequestUpdates`
/// message arriving over the wire. Per the zero-trust boundary, all interning
/// of network-sourced names happens here in codec (never in the manager or
/// main), after `validate_identifier`. `listener_service` and `reply_to` are
/// network addresses, not identifiers, so they are not interned here.
///
/// Returns `(service, member, listener_def)` as interned `Symbol`s.
pub fn decode_request_updates(
    service: &str,
    member: &str,
    listener_def: &str,
    interner: &mut Interner,
) -> Result<(Symbol, Symbol, Symbol)> {
    validate_identifier(service)?;
    validate_identifier(member)?;
    validate_identifier(listener_def)?;
    Ok((
        interner.insert(service),
        interner.insert(member),
        interner.insert(listener_def),
    ))
}

/// #24: validate and intern the identifier fields of an `Update` message
/// arriving over the wire (the `value` is decoded separately via
/// `decode_value`). As with `decode_request_updates`, all interning of
/// network-sourced names happens here after validation.
///
/// Returns `(listener_def, source_service, member)` as interned `Symbol`s.
pub fn decode_update(
    listener_def: &str,
    source_service: &str,
    member: &str,
    interner: &mut Interner,
) -> Result<(Symbol, Symbol, Symbol)> {
    validate_identifier(listener_def)?;
    validate_identifier(source_service)?;
    validate_identifier(member)?;
    Ok((
        interner.insert(listener_def),
        interner.insert(source_service),
        interner.insert(member),
    ))
}

/// Encode a runtime `Type` using recursion depth accumulator
///
/// Args:
///     ty (`&Type`): The runtime type to encode
///     depth (`usize`): The current recursion depth
///
/// Returns:
///     `Result<NetType>`: The encoded network type representation
///
/// Raises:
///     `Error::LimitExceeded`: The type nesting depth exceeds
///     maximum limit
fn encode_type_internal(ty: &Type, depth: usize) -> Result<NetType> {
    if depth > MAX_TYPE_DEPTH {
        return Err(Error::LimitExceeded(format!(
            "Type nesting depth exceeds maximum limit of {}",
            MAX_TYPE_DEPTH
        )));
    }
    match ty {
        Type::Int => Ok(NetType::Int),
        Type::String => Ok(NetType::String),
        Type::Bool => Ok(NetType::Bool),
        Type::Unit => Ok(NetType::Unit),
        Type::Tuple(ts) => {
            debug_assert!(ts.len() >= 2);
            let mut encoded_ts = Vec::new();
            for t in ts.iter() {
                encoded_ts.push(encode_type_internal(t, depth + 1)?);
            }
            Ok(NetType::Tuple(encoded_ts))
        }
        Type::Func(t1, t2, _) => {
            let et1 = encode_type_internal(t1, depth + 1)?;
            let et2 = encode_type_internal(t2, depth + 1)?;
            Ok(NetType::Func(Box::new(et1), Box::new(et2)))
        }
        Type::List(t) => {
            let et = encode_type_internal(t, depth + 1)?;
            Ok(NetType::List(Box::new(et)))
        }
        Type::UnresolvedService(_) => Err(Error::Message(
            "Cannot serialize unresolved service type".to_string(),
        )),
    }
}

/// Encode a runtime `Type` into a network representation
///
/// Enforces type depth limit to prevent stack-overflow DoS attacks
///
/// Args:
///     ty (`&Type`): The runtime type to encode
///
/// Returns:
///     `Result<NetType>`: The encoded network type representation
///
/// Raises:
///     `Error::LimitExceeded`: The type nesting depth exceeds
///     maximum limit
pub fn encode_type(ty: &Type) -> Result<NetType> {
    encode_type_internal(ty, 0)
}

/// Encode a runtime `ServiceType` into its network representation
///
/// Args:
///     `st` (`&ServiceType`): The runtime service type to encode
///     `interner` (`&Interner`): The interner for symbol lookup
///
/// Returns:
///     `Result<NetServiceType>`: The encoded network service type
///
/// Raises:
///     `Error::Message`: If a field is missing due to a broken invariant
pub fn encode_servicetype(st: &ServiceType, interner: &Interner) -> Result<NetServiceType> {
    let mut fields = Vec::new();
    for name in st.field_order() {
        let name_str = interner.get(*name).to_string();
        // While `ServiceType` encapsulation theoretically guarantees
        // that `field_order` entries exist in `fields`, we
        // defensively return an `Error::Message` instead of
        // panicking to ensure the network service fails
        // gracefully if an internal bug occurs
        let field_ty = st.fields().find(*name).ok_or_else(|| {
            Error::Message(format!(
                "Internal invariant broken: field_order \
                     entry missing from fields map: {}",
                name_str
            ))
        })?;
        let net_ty = encode_type(field_ty)?;
        fields.push((name_str, net_ty));
    }
    Ok(NetServiceType { fields })
}

/// Decode a network `NetType` with recursion depth accumulator
///
/// Args:
///     ty (`NetType`): The network type to decode
///     depth (`usize`): The current recursion depth
///     interner (`&mut Interner`): The interner for interning symbols
///
/// Returns:
///     `Result<Type>`: The decoded runtime type
///
/// Raises:
///     `Error::LimitExceeded`: The type nesting depth exceeds
///     maximum limit
fn decode_type_internal(ty: NetType, depth: usize) -> Result<Type> {
    if depth > MAX_TYPE_DEPTH {
        return Err(Error::LimitExceeded(format!(
            "Type nesting depth exceeds maximum limit of {}",
            MAX_TYPE_DEPTH
        )));
    }
    match ty {
        NetType::Int => Ok(Type::Int),
        NetType::String => Ok(Type::String),
        NetType::Bool => Ok(Type::Bool),
        NetType::Unit => Ok(Type::Unit),
        NetType::Tuple(ts) => {
            if ts.len() < 2 {
                return Err(Error::Message(format!(
                    "invalid network tuple arity: {}",
                    ts.len()
                )));
            }
            let mut decoded_ts = Vec::new();
            for t in ts {
                decoded_ts.push(decode_type_internal(t, depth + 1)?);
            }
            let tuple_type =
                TupleType::new(decoded_ts).map_err(|e| Error::Message(e.to_string()))?;
            Ok(Type::Tuple(tuple_type))
        }
        NetType::Func(t1, t2) => {
            let dt1 = decode_type_internal(*t1, depth + 1)?;
            let dt2 = decode_type_internal(*t2, depth + 1)?;
            Ok(Type::Func(
                Box::new(dt1),
                Box::new(dt2),
                std::collections::HashSet::new(),
            ))
        }
        NetType::List(t) => {
            let dt = decode_type_internal(*t, depth + 1)?;
            Ok(Type::List(Box::new(dt)))
        }
    }
}

/// Decode a network `NetType` representation into a runtime `Type`
///
/// Enforces type depth limit to prevent stack-overflow DoS attacks
///
/// Args:
///     ty (`NetType`): The network type to decode
///
/// Returns:
///     `Result<Type>`: The decoded runtime type
///
/// Raises:
///     `Error::LimitExceeded`: The type nesting depth exceeds
///     maximum limit
pub fn decode_type(ty: NetType) -> Result<Type> {
    decode_type_internal(ty, 0)
}

/// Decode a network `NetServiceType` into a runtime `ServiceType`
///
/// Args:
///     nst (`NetServiceType`): The network service type to decode
///     interner (`&mut Interner`): The interner for interning symbols
///
/// Returns:
///     `Result<ServiceType>`: The decoded runtime service type
pub fn decode_servicetype(nst: NetServiceType, interner: &mut Interner) -> Result<ServiceType> {
    let mut st = ServiceType::default();
    for (name_str, net_ty) in nst.fields {
        validate_identifier(&name_str)?;
        let sym = interner.insert(&name_str);
        let ty = decode_type(net_ty)?;
        st.add_field(sym, ty).map_err(|_| {
            Error::Message(format!(
                "duplicate field name '{}' in ServiceType",
                name_str
            ))
        })?;
    }
    Ok(st)
}

/// Encode a runtime `Param` into a network representation
///
/// Args:
///     `param` (`&Param`): The runtime parameter to encode
///     `interner` (`&Interner`): The interner for symbol lookup
///
/// Returns:
///     `Result<NetParam>`: The encoded network parameter
///     representation
pub fn encode_param(param: &Param, interner: &Interner) -> Result<NetParam> {
    let name_str = interner.get(param.name);
    validate_identifier(name_str)?;
    let ty = param.ty.as_ref().map(encode_type).transpose()?;
    Ok(NetParam {
        name: name_str.to_string(),
        ty,
    })
}

/// Decode a network `NetParam` representation into a runtime `Param`
///
/// Args:
///     `param` (`NetParam`): The network parameter to decode
///     `interner` (`&mut Interner`): The interner for symbol
///     creation
///
/// Returns:
///     `Result<Param>`: The decoded runtime parameter
pub fn decode_param(param: NetParam, interner: &mut Interner) -> Result<Param> {
    validate_identifier(&param.name)?;
    let ty = param.ty.map(decode_type).transpose()?;
    Ok(Param {
        name: interner.insert(&param.name),
        ty,
    })
}

/// Encode a runtime `Value` into a network representation
///
/// Args:
///     val (`&Value`): The runtime `Value` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `Result<NetValue>`: The encoded `NetValue` network representation
pub fn encode_value(val: &Value, interner: &Interner) -> Result<NetValue> {
    match val {
        Value::Int { val } => Ok(NetValue::Int { val: *val }),
        Value::Bool { val } => Ok(NetValue::Bool { val: *val }),
        // HTML has no network representation yet. A structured, validatable
        // wire form is deferred to the codec work in the code-transport PR.
        Value::Html(_) => Err(Error::Message(
            "html values cannot yet be sent over the network".to_string(),
        )),
        Value::String { val } => {
            validate_string_literal(val)?;
            Ok(NetValue::String { val: val.clone() })
        }
        Value::Closure {
            params,
            body,
            env,
            service_name,
            return_ty,
        } => {
            let mut encoded_params = Vec::new();
            for p in params {
                encoded_params.push(encode_param(p, interner)?);
            }
            let encoded_body = Box::new(encode_expr(body, interner)?);
            let mut encoded_env = Vec::new();
            for (k, v) in env {
                let k_str = interner.get(*k);
                validate_identifier(k_str)?;
                encoded_env.push((k_str.to_string(), encode_value(v, interner)?));
            }
            let service_str = interner.get(*service_name);
            validate_identifier(service_str)?;
            let encoded_return_ty = return_ty.as_ref().map(encode_type).transpose()?;
            Ok(NetValue::Closure {
                params: encoded_params,
                body: encoded_body,
                env: encoded_env,
                service_name: service_str.to_string(),
                return_ty: encoded_return_ty,
            })
        }
        Value::ActionClosure {
            stmts,
            env,
            service_net_id,
        } => {
            let mut encoded_stmts = Vec::new();
            for s in stmts {
                encoded_stmts.push(encode_action_stmt(s, interner)?);
            }
            let mut encoded_env = Vec::new();
            for (k, v) in env {
                let k_str = interner.get(*k);
                validate_identifier(k_str)?;
                encoded_env.push((k_str.to_string(), encode_value(v, interner)?));
            }
            Ok(NetValue::ActionClosure {
                stmts: encoded_stmts,
                env: encoded_env,
                service_net_id: service_net_id.clone(),
            })
        }
        Value::List { vals } => {
            let mut encoded = Vec::new();
            for v in vals {
                encoded.push(encode_value(v, interner)?);
            }
            Ok(NetValue::List { vals: encoded })
        }
        Value::Range { start, end } => Ok(NetValue::Range {
            start: *start,
            end: *end,
        }),
    }
}

/// Decode a network `NetValue` representation into a runtime `Value`
///
/// Args:
///     val (`NetValue`): The network `NetValue` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Result<Value>`: The decoded runtime `Value`
pub fn decode_value(val: NetValue, interner: &mut Interner) -> Result<Value> {
    match val {
        NetValue::Int { val } => Ok(Value::Int { val }),
        NetValue::Bool { val } => Ok(Value::Bool { val }),
        NetValue::String { val } => {
            validate_string_literal(&val)?;
            Ok(Value::String { val })
        }
        NetValue::Closure {
            params,
            body,
            env,
            service_name,
            return_ty,
        } => {
            let mut decoded_params = Vec::new();
            for p in params {
                decoded_params.push(decode_param(p, interner)?);
            }
            let decoded_body = Box::new(decode_expr(*body, interner)?);
            let mut decoded_env = Vec::new();
            for (k, v) in env {
                validate_identifier(&k)?;
                decoded_env.push((interner.insert(&k), decode_value(v, interner)?));
            }
            validate_identifier(&service_name)?;
            let decoded_service = interner.insert(&service_name);
            let decoded_return_ty = return_ty.map(decode_type).transpose()?;
            Ok(Value::Closure {
                params: decoded_params,
                body: decoded_body,
                env: decoded_env,
                service_name: decoded_service,
                return_ty: decoded_return_ty,
            })
        }
        NetValue::ActionClosure {
            stmts,
            env,
            service_net_id,
        } => {
            let mut decoded_stmts = Vec::new();
            for s in stmts {
                decoded_stmts.push(decode_action_stmt(s, interner)?);
            }
            let mut decoded_env = Vec::new();
            for (k, v) in env {
                validate_identifier(&k)?;
                decoded_env.push((interner.insert(&k), decode_value(v, interner)?));
            }
            Ok(Value::ActionClosure {
                stmts: decoded_stmts,
                env: decoded_env,
                service_net_id,
            })
        }
        NetValue::List { vals } => {
            let mut decoded = Vec::new();
            for v in vals {
                decoded.push(decode_value(v, interner)?);
            }
            Ok(Value::List { vals: decoded })
        }
        NetValue::Range { start, end } => Ok(Value::Range { start, end }),
    }
}

/// Encode a runtime `Expr` into a network representation
///
/// Args:
///     expr (`&Expr`): The runtime `Expr` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `Result<NetExpr>`: The encoded `NetExpr` network representation
pub fn encode_expr(expr: &Expr, interner: &Interner) -> Result<NetExpr> {
    match expr {
        Expr::Literal { val } => Ok(NetExpr::Literal {
            val: encode_value(val, interner)?,
        }),
        // HTML templates have no network representation yet; deferred to the
        // code-transport PR alongside the html value wire form.
        Expr::Html { .. } => Err(Error::Message(
            "html expressions cannot yet be sent over the network".to_string(),
        )),
        Expr::Variable { name } => {
            let name_str = interner.get(*name);
            validate_identifier(name_str)?;
            Ok(NetExpr::Variable {
                name: name_str.to_string(),
            })
        }
        Expr::Tuple { val } => {
            let mut encoded_val = Vec::new();
            for e in val {
                encoded_val.push(encode_expr(e, interner)?);
            }
            Ok(NetExpr::Tuple { val: encoded_val })
        }
        Expr::KeyVal { name, value } => {
            let name_str = interner.get(*name);
            validate_identifier(name_str)?;
            Ok(NetExpr::KeyVal {
                name: name_str.to_string(),
                value: Box::new(encode_expr(value, interner)?),
            })
        }
        Expr::Unop { op, expr } => Ok(NetExpr::Unop {
            op: encode_unop(*op),
            expr: Box::new(encode_expr(expr, interner)?),
        }),
        Expr::Binop { op, expr1, expr2 } => Ok(NetExpr::Binop {
            op: encode_binop(*op),
            expr1: Box::new(encode_expr(expr1, interner)?),
            expr2: Box::new(encode_expr(expr2, interner)?),
        }),
        Expr::If { cond, expr1, expr2 } => Ok(NetExpr::If {
            cond: Box::new(encode_expr(cond, interner)?),
            expr1: Box::new(encode_expr(expr1, interner)?),
            expr2: Box::new(encode_expr(expr2, interner)?),
        }),
        Expr::Func {
            params,
            body,
            return_ty,
        } => {
            let mut encoded_params = Vec::new();
            for p in params {
                encoded_params.push(encode_param(p, interner)?);
            }
            let encoded_body = Box::new(encode_expr(body, interner)?);
            let encoded_return_ty = return_ty.as_ref().map(encode_type).transpose()?;
            Ok(NetExpr::Func {
                params: encoded_params,
                body: encoded_body,
                return_ty: encoded_return_ty,
            })
        }
        Expr::Call { func, args } => {
            let encoded_func = Box::new(encode_expr(func, interner)?);
            let mut encoded_args = Vec::new();
            for e in args {
                encoded_args.push(encode_expr(e, interner)?);
            }
            Ok(NetExpr::Call {
                func: encoded_func,
                args: encoded_args,
            })
        }
        Expr::Action(stmts) => {
            let mut encoded_stmts = Vec::new();
            for s in stmts {
                encoded_stmts.push(encode_action_stmt(s, interner)?);
            }
            Ok(NetExpr::Action(encoded_stmts))
        }
        Expr::MemberAccess {
            service_name,
            member_name,
        } => {
            let service_str = interner.get(*service_name);
            let member_str = interner.get(*member_name);
            validate_identifier(service_str)?;
            validate_identifier(member_str)?;
            Ok(NetExpr::MemberAccess {
                service_name: service_str.to_string(),
                member_name: member_str.to_string(),
            })
        }
        Expr::Select {
            table_name,
            column_names,
            where_clause,
        } => {
            let table_str = interner.get(*table_name);
            validate_identifier(table_str)?;
            let mut encoded_cols = Vec::new();
            for c in column_names {
                let c_str = interner.get(*c);
                validate_identifier(c_str)?;
                encoded_cols.push(c_str.to_string());
            }
            Ok(NetExpr::Select {
                table_name: table_str.to_string(),
                column_names: encoded_cols,
                where_clause: Box::new(encode_expr(where_clause, interner)?),
            })
        }
        Expr::Table { schema, records } => {
            let mut encoded_schema = Vec::new();
            for f in schema {
                encoded_schema.push(encode_field(f, interner)?);
            }
            let mut encoded_records = Vec::new();
            for r in records {
                encoded_records.push(encode_expr(r, interner)?);
            }
            Ok(NetExpr::Table {
                schema: encoded_schema,
                records: encoded_records,
            })
        }
        Expr::Fold {
            table_name,
            column_name,
            operation,
            identity,
        } => {
            let table_str = interner.get(*table_name);
            let column_str = interner.get(*column_name);
            validate_identifier(table_str)?;
            validate_identifier(column_str)?;
            Ok(NetExpr::Fold {
                table_name: table_str.to_string(),
                column_name: column_str.to_string(),
                operation: Box::new(encode_expr(operation, interner)?),
                identity: Box::new(encode_expr(identity, interner)?),
            })
        }
        Expr::List(exprs) => {
            let mut encoded = Vec::new();
            for e in exprs {
                encoded.push(encode_expr(e, interner)?);
            }
            Ok(NetExpr::List(encoded))
        }
        Expr::Range { start, end } => Ok(NetExpr::Range {
            start: Box::new(encode_expr(start, interner)?),
            end: Box::new(encode_expr(end, interner)?),
        }),
    }
}

/// Decode a network `NetExpr` representation into a runtime `Expr`
///
/// Args:
///     expr (`NetExpr`): The network `NetExpr` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Result<Expr>`: The decoded runtime `Expr`
pub fn decode_expr(expr: NetExpr, interner: &mut Interner) -> Result<Expr> {
    match expr {
        NetExpr::Literal { val } => Ok(Expr::Literal {
            val: decode_value(val, interner)?,
        }),
        NetExpr::Variable { name } => {
            validate_identifier(&name)?;
            Ok(Expr::Variable {
                name: interner.insert(&name),
            })
        }
        NetExpr::Tuple { val } => {
            let mut decoded_val = Vec::new();
            for e in val {
                decoded_val.push(decode_expr(e, interner)?);
            }
            Ok(Expr::Tuple { val: decoded_val })
        }
        NetExpr::KeyVal { name, value } => {
            validate_identifier(&name)?;
            Ok(Expr::KeyVal {
                name: interner.insert(&name),
                value: Box::new(decode_expr(*value, interner)?),
            })
        }
        NetExpr::Unop { op, expr } => Ok(Expr::Unop {
            op: decode_unop(op),
            expr: Box::new(decode_expr(*expr, interner)?),
        }),
        NetExpr::Binop { op, expr1, expr2 } => Ok(Expr::Binop {
            op: decode_binop(op),
            expr1: Box::new(decode_expr(*expr1, interner)?),
            expr2: Box::new(decode_expr(*expr2, interner)?),
        }),
        NetExpr::If { cond, expr1, expr2 } => Ok(Expr::If {
            cond: Box::new(decode_expr(*cond, interner)?),
            expr1: Box::new(decode_expr(*expr1, interner)?),
            expr2: Box::new(decode_expr(*expr2, interner)?),
        }),
        NetExpr::Func {
            params,
            body,
            return_ty,
        } => {
            let mut decoded_params = Vec::new();
            for p in params {
                decoded_params.push(decode_param(p, interner)?);
            }
            let decoded_body = Box::new(decode_expr(*body, interner)?);
            let decoded_return_ty = return_ty.map(decode_type).transpose()?;
            Ok(Expr::Func {
                params: decoded_params,
                body: decoded_body,
                return_ty: decoded_return_ty,
            })
        }
        NetExpr::Call { func, args } => {
            let decoded_func = Box::new(decode_expr(*func, interner)?);
            let mut decoded_args = Vec::new();
            for e in args {
                decoded_args.push(decode_expr(e, interner)?);
            }
            Ok(Expr::Call {
                func: decoded_func,
                args: decoded_args,
            })
        }
        NetExpr::Action(stmts) => {
            let mut decoded_stmts = Vec::new();
            for s in stmts {
                decoded_stmts.push(decode_action_stmt(s, interner)?);
            }
            Ok(Expr::Action(decoded_stmts))
        }
        NetExpr::MemberAccess {
            service_name,
            member_name,
        } => {
            validate_identifier(&service_name)?;
            validate_identifier(&member_name)?;
            Ok(Expr::MemberAccess {
                service_name: interner.insert(&service_name),
                member_name: interner.insert(&member_name),
            })
        }
        NetExpr::Select {
            table_name,
            column_names,
            where_clause,
        } => {
            validate_identifier(&table_name)?;
            for c in &column_names {
                validate_identifier(c)?;
            }
            let decoded_cols = column_names
                .into_iter()
                .map(|c| interner.insert(&c))
                .collect();
            Ok(Expr::Select {
                table_name: interner.insert(&table_name),
                column_names: decoded_cols,
                where_clause: Box::new(decode_expr(*where_clause, interner)?),
            })
        }
        NetExpr::Table { schema, records } => {
            let mut decoded_schema = Vec::new();
            for f in schema {
                decoded_schema.push(decode_field(f, interner)?);
            }
            let mut decoded_records = Vec::new();
            for r in records {
                decoded_records.push(decode_expr(r, interner)?);
            }
            Ok(Expr::Table {
                schema: decoded_schema,
                records: decoded_records,
            })
        }
        NetExpr::Fold {
            table_name,
            column_name,
            operation,
            identity,
        } => {
            validate_identifier(&table_name)?;
            validate_identifier(&column_name)?;
            Ok(Expr::Fold {
                table_name: interner.insert(&table_name),
                column_name: interner.insert(&column_name),
                operation: Box::new(decode_expr(*operation, interner)?),
                identity: Box::new(decode_expr(*identity, interner)?),
            })
        }
        NetExpr::List(exprs) => {
            let mut decoded = Vec::new();
            for e in exprs {
                decoded.push(decode_expr(e, interner)?);
            }
            Ok(Expr::List(decoded))
        }
        NetExpr::Range { start, end } => Ok(Expr::Range {
            start: Box::new(decode_expr(*start, interner)?),
            end: Box::new(decode_expr(*end, interner)?),
        }),
    }
}

/// Encode a runtime `ActionStmt` into a network representation
///
/// Args:
///     stmt (`&ActionStmt`): The runtime `ActionStmt` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `Result<NetActionStmt>`: The encoded `NetActionStmt` network
///     representation
pub fn encode_action_stmt(stmt: &ActionStmt, interner: &Interner) -> Result<NetActionStmt> {
    match stmt {
        ActionStmt::Let { name, ty, expr } => {
            let name_str = interner.get(*name);
            validate_identifier(name_str)?;
            let encoded_ty = ty.as_ref().map(encode_type).transpose()?;
            Ok(NetActionStmt::Let {
                name: name_str.to_string(),
                ty: encoded_ty,
                expr: encode_expr(expr, interner)?,
            })
        }
        ActionStmt::Expr(expr) => Ok(NetActionStmt::Expr(encode_expr(expr, interner)?)),
        ActionStmt::Do(expr) => Ok(NetActionStmt::Do(encode_expr(expr, interner)?)),
        ActionStmt::Assert(expr, text) => {
            validate_string_literal(text)?;
            Ok(NetActionStmt::Assert(
                encode_expr(expr, interner)?,
                text.clone(),
            ))
        }
        ActionStmt::Assign { name, expr } => {
            let name_str = interner.get(*name);
            validate_identifier(name_str)?;
            Ok(NetActionStmt::Assign {
                name: name_str.to_string(),
                expr: encode_expr(expr, interner)?,
            })
        }
        ActionStmt::Insert { row, table_name } => {
            let table_str = interner.get(*table_name);
            validate_identifier(table_str)?;
            Ok(NetActionStmt::Insert {
                row: encode_expr(row, interner)?,
                table_name: table_str.to_string(),
            })
        }
        ActionStmt::For {
            var,
            iterable,
            body,
        } => {
            let var_str = interner.get(*var).to_string();
            validate_identifier(&var_str)?;
            let encoded_iterable = encode_expr(iterable, interner)?;
            let mut encoded_body = Vec::new();
            for s in body {
                encoded_body.push(encode_action_stmt(s, interner)?);
            }
            Ok(NetActionStmt::For {
                var: var_str,
                iterable: encoded_iterable,
                body: encoded_body,
            })
        }
    }
}

/// Decode a network `NetActionStmt` into a runtime `ActionStmt`
///
/// Args:
///     stmt (`NetActionStmt`): The network `NetActionStmt` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Result<ActionStmt>`: The decoded runtime `ActionStmt`
pub fn decode_action_stmt(stmt: NetActionStmt, interner: &mut Interner) -> Result<ActionStmt> {
    match stmt {
        NetActionStmt::Let { name, ty, expr } => {
            validate_identifier(&name)?;
            let decoded_ty = ty.map(decode_type).transpose()?;
            Ok(ActionStmt::Let {
                name: interner.insert(&name),
                ty: decoded_ty,
                expr: decode_expr(expr, interner)?,
            })
        }
        NetActionStmt::Expr(expr) => Ok(ActionStmt::Expr(decode_expr(expr, interner)?)),
        NetActionStmt::Do(expr) => Ok(ActionStmt::Do(decode_expr(expr, interner)?)),
        NetActionStmt::Assert(expr, text) => {
            validate_string_literal(&text)?;
            Ok(ActionStmt::Assert(decode_expr(expr, interner)?, text))
        }
        NetActionStmt::Assign { name, expr } => {
            validate_identifier(&name)?;
            Ok(ActionStmt::Assign {
                name: interner.insert(&name),
                expr: decode_expr(expr, interner)?,
            })
        }
        NetActionStmt::Insert { row, table_name } => {
            validate_identifier(&table_name)?;
            Ok(ActionStmt::Insert {
                row: decode_expr(row, interner)?,
                table_name: interner.insert(&table_name),
            })
        }
        NetActionStmt::For {
            var,
            iterable,
            body,
        } => {
            validate_identifier(&var)?;
            let decoded_iterable = decode_expr(iterable, interner)?;
            let mut decoded_body = Vec::new();
            for s in body {
                decoded_body.push(decode_action_stmt(s, interner)?);
            }
            Ok(ActionStmt::For {
                var: interner.insert(&var),
                iterable: decoded_iterable,
                body: decoded_body,
            })
        }
    }
}

/// Encode a runtime `Field` into a network representation
///
/// Args:
///     field (`&Field`): The runtime `Field` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `Result<NetField>`: The encoded `NetField` network representation
pub fn encode_field(field: &Field, interner: &Interner) -> Result<NetField> {
    let name_str = interner.get(field.name);
    validate_identifier(name_str)?;
    Ok(NetField {
        name: name_str.to_string(),
        ty: encode_tabletype(&field.ty),
    })
}

/// Decode a network `NetField` representation into a runtime `Field`
///
/// Args:
///     field (`NetField`): The network `NetField` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Result<Field>`: The decoded runtime `Field`
pub fn decode_field(field: NetField, interner: &mut Interner) -> Result<Field> {
    validate_identifier(&field.name)?;
    Ok(Field {
        name: interner.insert(&field.name),
        ty: decode_tabletype(field.ty),
    })
}

/// Encode a runtime `UnOp` into its network equivalent
///
/// Args:
///     op (`UnOp`): The runtime operator to encode
///
/// Returns:
///     `NetUnOp`: The encoded network operator representation
pub fn encode_unop(op: UnOp) -> NetUnOp {
    match op {
        UnOp::Neg => NetUnOp::Neg,
        UnOp::Not => NetUnOp::Not,
    }
}

/// Decode a network `NetUnOp` into its runtime equivalent
///
/// Args:
///     op (`NetUnOp`): The network operator to decode
///
/// Returns:
///     `UnOp`: The decoded runtime operator representation
pub fn decode_unop(op: NetUnOp) -> UnOp {
    match op {
        NetUnOp::Neg => UnOp::Neg,
        NetUnOp::Not => UnOp::Not,
    }
}

/// Encode a runtime `BinOp` into its network equivalent
///
/// Args:
///     op (`BinOp`): The runtime operator to encode
///
/// Returns:
///     `NetBinOp`: The encoded network operator representation
pub fn encode_binop(op: BinOp) -> NetBinOp {
    match op {
        BinOp::Add => NetBinOp::Add,
        BinOp::Sub => NetBinOp::Sub,
        BinOp::Mul => NetBinOp::Mul,
        BinOp::Div => NetBinOp::Div,
        BinOp::Eq => NetBinOp::Eq,
        BinOp::Lt => NetBinOp::Lt,
        BinOp::Gt => NetBinOp::Gt,
        BinOp::And => NetBinOp::And,
        BinOp::Or => NetBinOp::Or,
    }
}

/// Decode a network `NetBinOp` into its runtime equivalent
///
/// Args:
///     op (`NetBinOp`): The network operator to decode
///
/// Returns:
///     `BinOp`: The decoded runtime operator representation
pub fn decode_binop(op: NetBinOp) -> BinOp {
    match op {
        NetBinOp::Add => BinOp::Add,
        NetBinOp::Sub => BinOp::Sub,
        NetBinOp::Mul => BinOp::Mul,
        NetBinOp::Div => BinOp::Div,
        NetBinOp::Eq => BinOp::Eq,
        NetBinOp::Lt => BinOp::Lt,
        NetBinOp::Gt => BinOp::Gt,
        NetBinOp::And => BinOp::And,
        NetBinOp::Or => BinOp::Or,
    }
}

/// Encode a runtime `TableType` into its network equivalent
///
/// Args:
///     t (`&TableType`): The runtime table type to encode
///
/// Returns:
///     `NetTableType`: The encoded network table type representation
pub fn encode_tabletype(t: &TableType) -> NetTableType {
    match t {
        TableType::String => NetTableType::String,
        TableType::Int => NetTableType::Int,
        TableType::Bool => NetTableType::Bool,
    }
}

/// Decode a network `NetTableType` into its runtime equivalent
///
/// Args:
///     t (`NetTableType`): The network table type to decode
///
/// Returns:
///     `TableType`: The decoded runtime table type representation
pub fn decode_tabletype(t: NetTableType) -> TableType {
    match t {
        NetTableType::String => TableType::String,
        NetTableType::Int => TableType::Int,
        NetTableType::Bool => TableType::Bool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::ServiceNetId;

    /// Verify round-trip encoding, serialization, deserialization, and decoding of `AST` types
    #[test]
    fn test_value_codec_roundtrip() {
        let mut interner_orig = Interner::new();
        let service_net_id = ServiceNetId::new("test_service");

        let var_x = interner_orig.insert("x");
        let tbl_t = interner_orig.insert("t");

        let stmt1 = ActionStmt::Let {
            name: var_x,
            ty: Some(Type::Int),
            expr: Expr::Literal {
                val: Value::Int { val: 42 },
            },
        };
        let stmt2 = ActionStmt::Insert {
            row: Expr::Variable { name: var_x },
            table_name: tbl_t,
        };

        let env_var = interner_orig.insert("y");
        let env = vec![(env_var, Value::Bool { val: true })];

        let original_value = Value::ActionClosure {
            stmts: vec![stmt1, stmt2],
            env,
            service_net_id,
        };

        let orig_str = format!("{}", original_value);

        let encoded = encode_value(&original_value, &interner_orig).unwrap();

        let json_str = serde_json::to_string(&encoded).unwrap();
        let decoded_net_val: NetValue = serde_json::from_str(&json_str).unwrap();

        let mut interner_new = Interner::new();
        let decoded_value = decode_value(decoded_net_val, &mut interner_new).unwrap();

        let new_str = format!("{}", decoded_value);

        assert_eq!(orig_str, new_str);
    }

    /// Verify round-trip encoding and decoding for
    /// Value::String and Value::Closure
    #[test]
    fn test_value_codec_exhaustive() {
        let mut interner_orig = Interner::new();
        let param_name = interner_orig.insert("x");
        let body = Expr::Literal {
            val: Value::String {
                val: "hello".to_string(),
            },
        };
        let env_key = interner_orig.insert("y");
        let env_val = Value::Int { val: 123 };
        let service = interner_orig.insert("my_service");

        let original_value = Value::Closure {
            params: vec![Param {
                name: param_name,
                ty: Some(Type::String),
            }],
            body: Box::new(body),
            env: vec![(env_key, env_val)],
            service_name: service,
            return_ty: Some(Type::String),
        };

        let encoded = encode_value(&original_value, &interner_orig).unwrap();
        let mut interner_new = Interner::new();
        let decoded = decode_value(encoded, &mut interner_new).unwrap();

        assert_eq!(format!("{}", original_value), format!("{}", decoded));
    }

    /// Verify round-trip encoding and decoding for Tuple,
    /// KeyVal, Unop, Binop, and If expressions
    #[test]
    fn test_expr_codec_exhaustive_1() {
        let run_expr_test = |expr: &Expr, interner_orig: &Interner| {
            let encoded = encode_expr(expr, interner_orig).unwrap();
            let mut interner_new = Interner::new();
            let decoded = decode_expr(encoded, &mut interner_new).unwrap();
            assert_eq!(format!("{}", expr), format!("{}", decoded));
        };

        // 1. Tuple
        let interner = Interner::new();
        let tuple_expr = Expr::Tuple {
            val: vec![
                Expr::Literal {
                    val: Value::Int { val: 1 },
                },
                Expr::Literal {
                    val: Value::Int { val: 2 },
                },
            ],
        };
        run_expr_test(&tuple_expr, &interner);

        // 2. KeyVal
        let mut interner = Interner::new();
        let name_kv = interner.insert("kv_name");
        let key_val_expr = Expr::KeyVal {
            name: name_kv,
            value: Box::new(Expr::Literal {
                val: Value::Int { val: 3 },
            }),
        };
        run_expr_test(&key_val_expr, &interner);

        // 3. Unop (Neg, Not)
        for op in &[UnOp::Neg, UnOp::Not] {
            let interner = Interner::new();
            let unop_expr = Expr::Unop {
                op: *op,
                expr: Box::new(Expr::Literal {
                    val: Value::Bool { val: true },
                }),
            };
            run_expr_test(&unop_expr, &interner);
        }

        // 4. Binop
        let binops = &[
            BinOp::Add,
            BinOp::Sub,
            BinOp::Mul,
            BinOp::Div,
            BinOp::Eq,
            BinOp::Lt,
            BinOp::Gt,
            BinOp::And,
            BinOp::Or,
        ];
        for op in binops {
            let interner = Interner::new();
            let binop_expr = Expr::Binop {
                op: *op,
                expr1: Box::new(Expr::Literal {
                    val: Value::Int { val: 5 },
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 6 },
                }),
            };
            run_expr_test(&binop_expr, &interner);
        }

        // 5. If
        let interner = Interner::new();
        let if_expr = Expr::If {
            cond: Box::new(Expr::Literal {
                val: Value::Bool { val: true },
            }),
            expr1: Box::new(Expr::Literal {
                val: Value::Int { val: 7 },
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: 8 },
            }),
        };
        run_expr_test(&if_expr, &interner);
    }

    /// Verify round-trip encoding and decoding for Func,
    /// Call, Action, and MemberAccess expressions
    #[test]
    fn test_expr_codec_exhaustive_2() {
        let run_expr_test = |expr: &Expr, interner_orig: &Interner| {
            let encoded = encode_expr(expr, interner_orig).unwrap();
            let mut interner_new = Interner::new();
            let decoded = decode_expr(encoded, &mut interner_new).unwrap();
            assert_eq!(format!("{}", expr), format!("{}", decoded));
        };

        // 1. Func
        let mut interner = Interner::new();
        let param_name = interner.insert("p");
        let func_expr = Expr::Func {
            params: vec![Param {
                name: param_name,
                ty: Some(Type::Int),
            }],
            body: Box::new(Expr::Literal {
                val: Value::Int { val: 9 },
            }),
            return_ty: Some(Type::Int),
        };
        run_expr_test(&func_expr, &interner);

        // 2. Call
        let mut interner = Interner::new();
        let param_name = interner.insert("p");
        let func_expr = Expr::Func {
            params: vec![Param {
                name: param_name,
                ty: Some(Type::Int),
            }],
            body: Box::new(Expr::Literal {
                val: Value::Int { val: 9 },
            }),
            return_ty: Some(Type::Int),
        };
        let call_expr = Expr::Call {
            func: Box::new(func_expr),
            args: vec![Expr::Literal {
                val: Value::Int { val: 10 },
            }],
        };
        run_expr_test(&call_expr, &interner);

        // 3. MemberAccess
        let mut interner = Interner::new();
        let service_name = interner.insert("srv");
        let member_name = interner.insert("mem");
        let member_expr = Expr::MemberAccess {
            service_name,
            member_name,
        };
        run_expr_test(&member_expr, &interner);

        // 4. Action
        let interner = Interner::new();
        let action_expr = Expr::Action(vec![ActionStmt::Do(Expr::Literal {
            val: Value::Int { val: 11 },
        })]);
        run_expr_test(&action_expr, &interner);
    }

    /// Verify round-trip encoding and decoding for Select,
    /// Table, and Fold expressions
    #[test]
    fn test_expr_codec_exhaustive_3() {
        let run_expr_test = |expr: &Expr, interner_orig: &Interner| {
            let encoded = encode_expr(expr, interner_orig).unwrap();
            let mut interner_new = Interner::new();
            let decoded = decode_expr(encoded, &mut interner_new).unwrap();
            assert_eq!(format!("{}", expr), format!("{}", decoded));
        };

        // 1. Select
        let mut interner = Interner::new();
        let table_name = interner.insert("tbl");
        let col1 = interner.insert("col1");
        let col2 = interner.insert("col2");
        let select_expr = Expr::Select {
            table_name,
            column_names: vec![col1, col2],
            where_clause: Box::new(Expr::Literal {
                val: Value::Bool { val: true },
            }),
        };
        run_expr_test(&select_expr, &interner);

        // 2. Table
        let mut interner = Interner::new();
        let col1 = interner.insert("col1");
        let col2 = interner.insert("col2");
        let f1 = Field {
            name: col1,
            ty: TableType::String,
        };
        let f2 = Field {
            name: col2,
            ty: TableType::Int,
        };
        let f3 = Field {
            name: col2,
            ty: TableType::Bool,
        };
        let table_expr = Expr::Table {
            schema: vec![f1, f2, f3],
            records: vec![Expr::Literal {
                val: Value::String {
                    val: "abc".to_string(),
                },
            }],
        };
        run_expr_test(&table_expr, &interner);

        // 3. Fold
        let mut interner = Interner::new();
        let table_name = interner.insert("tbl");
        let col1 = interner.insert("col1");
        let fold_expr = Expr::Fold {
            table_name,
            column_name: col1,
            operation: Box::new(Expr::Literal {
                val: Value::Int { val: 42 },
            }),
            identity: Box::new(Expr::Literal {
                val: Value::Int { val: 0 },
            }),
        };
        run_expr_test(&fold_expr, &interner);
    }

    /// Verify round-trip encoding and decoding for Expr,
    /// Do, Assert, and Assign ActionStmts
    #[test]
    fn test_action_stmt_codec_exhaustive() {
        let run_stmt_test = |stmt: &ActionStmt, interner_orig: &Interner| {
            let encoded = encode_action_stmt(stmt, interner_orig).unwrap();
            let mut interner_new = Interner::new();
            let decoded = decode_action_stmt(encoded, &mut interner_new).unwrap();
            assert_eq!(format!("{}", stmt), format!("{}", decoded));
        };

        // 1. Expr
        let interner = Interner::new();
        let stmt_expr = ActionStmt::Expr(Expr::Literal {
            val: Value::Int { val: 100 },
        });
        run_stmt_test(&stmt_expr, &interner);

        // 2. Do
        let interner = Interner::new();
        let stmt_do = ActionStmt::Do(Expr::Literal {
            val: Value::Int { val: 200 },
        });
        run_stmt_test(&stmt_do, &interner);

        // 3. Assert
        let interner = Interner::new();
        let stmt_assert = ActionStmt::Assert(
            Expr::Literal {
                val: Value::Bool { val: true },
            },
            "true".to_string(),
        );
        run_stmt_test(&stmt_assert, &interner);

        // 4. Assign
        let mut interner = Interner::new();
        let name_var = interner.insert("v");
        let stmt_assign = ActionStmt::Assign {
            name: name_var,
            expr: Expr::Literal {
                val: Value::Int { val: 300 },
            },
        };
        run_stmt_test(&stmt_assign, &interner);
    }

    /// Verify that deserializing a structurally corrupted JSON
    /// string is rejected safely
    #[test]
    fn test_codec_corrupt_payload_rejection() {
        let malformed_json = "{ \"val\": { \"Closure\": { \"params\": [";
        let res: std::result::Result<NetValue, _> = serde_json::from_str(malformed_json);
        assert!(res.is_err());
    }

    /// Verify that type mismatches in JSON are rejected safely
    /// at the boundary
    #[test]
    fn test_codec_type_mismatch_rejection() {
        let mismatched_json = "{ \"Bool\": { \"val\": \"not_a_bool\" } }";
        let res: std::result::Result<NetValue, _> = serde_json::from_str(mismatched_json);
        assert!(res.is_err());
    }

    /// Verify that deeply nested AST structures do not crash the
    /// encoder or decoder
    #[test]
    fn test_codec_deeply_nested_structure() {
        let mut expr = Expr::Literal {
            val: Value::Int { val: 0 },
        };
        let mut interner = Interner::new();
        for _ in 0..20 {
            expr = Expr::Binop {
                op: BinOp::Add,
                expr1: Box::new(expr),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            };
        }

        let encoded = encode_expr(&expr, &interner).unwrap();
        let json_str = serde_json::to_string(&encoded).unwrap();
        let decoded_net: NetExpr = serde_json::from_str(&json_str).unwrap();
        let decoded = decode_expr(decoded_net, &mut interner).unwrap();

        assert_eq!(format!("{}", expr), format!("{}", decoded));
    }

    /// Verify that decoding a value with an oversized string
    /// literal fails
    #[test]
    fn test_codec_decode_oversized_string_literal() {
        let long_str = "a".repeat(MAX_STRING_LITERAL_LENGTH + 1);
        let net_val = NetValue::String { val: long_str };
        let mut interner = Interner::new();
        let res = decode_value(net_val, &mut interner);
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), Error::LimitExceeded(_)));
    }

    /// Verify that decoding a value with an oversized identifier
    /// fails
    #[test]
    fn test_codec_decode_oversized_identifier() {
        let long_ident = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        let net_val = NetValue::Closure {
            params: vec![NetParam {
                name: long_ident,
                ty: None,
            }],
            body: Box::new(NetExpr::Literal {
                val: NetValue::Int { val: 0 },
            }),
            env: vec![],
            service_name: "test".to_string(),
            return_ty: None,
        };
        let mut interner = Interner::new();
        let res = decode_value(net_val, &mut interner);
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), Error::LimitExceeded(_)));
    }

    /// #24: verify the codec rejects wire identifiers that the lexer would not
    /// accept locally (whitespace, punctuation, leading digit, empty), so a
    /// remote peer cannot grow the interner with non-identifier strings.
    #[test]
    fn test_codec_decode_rejects_non_identifier() {
        let mut interner = Interner::new();
        for bad in ["bad name", "bad!name", "1leading", "", "has.dot", "sp ace"] {
            let res = decode_request_updates(bad, "member", "ldef", &mut interner);
            assert!(
                res.is_err(),
                "decode_request_updates should reject service {:?}",
                bad
            );
            let res = decode_update("ldef", bad, "member", &mut interner);
            assert!(
                res.is_err(),
                "decode_update should reject source_service {:?}",
                bad
            );
        }
        // A well-formed identifier is accepted.
        assert!(decode_request_updates("svc", "y", "z", &mut interner).is_ok());
        assert!(decode_update("z", "svc", "y", &mut interner).is_ok());
    }

    /// Verify that encoding a value with an oversized string
    /// literal fails
    #[test]
    fn test_codec_encode_oversized_string_literal() {
        let long_str = "a".repeat(MAX_STRING_LITERAL_LENGTH + 1);
        let val = Value::String { val: long_str };
        let interner = Interner::new();
        let res = encode_value(&val, &interner);
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), Error::LimitExceeded(_)));
    }

    /// Verify round-trip encoding and decoding for
    /// assertion statement
    #[test]
    fn test_codec_assert_roundtrip() {
        let interner = Interner::new();
        let stmt = ActionStmt::Assert(
            Expr::Literal {
                val: Value::Bool { val: false },
            },
            "x == 5".to_string(),
        );
        let encoded = encode_action_stmt(&stmt, &interner).unwrap();
        let mut interner_new = Interner::new();
        let decoded = decode_action_stmt(encoded, &mut interner_new).unwrap();
        match decoded {
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                text,
            ) => assert_eq!(text, "x == 5"),
            other => panic!("unexpected decoded stmt: {:?}", other),
        }
    }

    /// Verify decoding an assertion with oversized text fails
    #[test]
    fn test_codec_decode_oversized_assert() {
        let long_str = "a".repeat(MAX_STRING_LITERAL_LENGTH + 1);
        let net_stmt = NetActionStmt::Assert(
            NetExpr::Literal {
                val: NetValue::Bool { val: false },
            },
            long_str,
        );
        let mut interner = Interner::new();
        let res = decode_action_stmt(net_stmt, &mut interner);
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), Error::LimitExceeded(_)));
    }

    /// Verify round-trip encoding and decoding for `NetType` and
    /// `NetParam` structures
    #[test]
    fn test_codec_type_and_param_roundtrip() {
        let mut interner_orig = Interner::new();
        let param_name = interner_orig.insert("param_x");

        // Construct `Type::Func(Int -> String, Bool -> Unit)`
        let original_type = Type::Func(
            Box::new(Type::Func(
                Box::new(Type::Int),
                Box::new(Type::String),
                std::collections::HashSet::new(),
            )),
            Box::new(Type::Func(
                Box::new(Type::Bool),
                Box::new(Type::Unit),
                std::collections::HashSet::new(),
            )),
            std::collections::HashSet::new(),
        );

        let original_param = Param {
            name: param_name,
            ty: Some(original_type.clone()),
        };

        let encoded_type = encode_type(&original_type).unwrap();
        let encoded_param = encode_param(&original_param, &interner_orig).unwrap();

        let mut interner_new = Interner::new();
        let decoded_type = decode_type(encoded_type).unwrap();
        let decoded_param = decode_param(encoded_param, &mut interner_new).unwrap();

        assert_eq!(original_type, decoded_type);
        assert_eq!(original_param.ty, decoded_param.ty);
    }

    /// Verify that deserializing a `NetType` exceeding
    /// `MAX_TYPE_DEPTH` returns a limit exceeded error
    #[test]
    fn test_codec_type_depth_overflow() {
        // Construct a `NetType` that exceeds `MAX_TYPE_DEPTH` (16)
        let mut current_type = NetType::Int;
        for _ in 0..(MAX_TYPE_DEPTH + 1) {
            current_type = NetType::Func(Box::new(NetType::Bool), Box::new(current_type));
        }

        let res = decode_type(current_type);
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), Error::LimitExceeded(_)));
    }

    /// Verify that encoding a `Type` exceeding `MAX_TYPE_DEPTH`
    /// returns a limit exceeded error
    #[test]
    fn test_codec_encode_type_depth_overflow() {
        // Construct a `Type` that exceeds `MAX_TYPE_DEPTH` (16)
        let mut current_type = Type::Int;
        for _ in 0..(MAX_TYPE_DEPTH + 1) {
            current_type = Type::Func(
                Box::new(Type::Bool),
                Box::new(current_type),
                std::collections::HashSet::new(),
            );
        }

        let res = encode_type(&current_type);
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), Error::LimitExceeded(_)));
    }

    /// Verify that `decode_type` rejects invalid network tuple arities
    #[test]
    fn test_codec_tuple_invalid_arity_decode() {
        // Zero elements tuple
        let zero_tuple = NetType::Tuple(vec![]);
        let res_zero = decode_type(zero_tuple);
        assert!(res_zero.is_err());
        assert!(matches!(res_zero.unwrap_err(), Error::Message(_)));

        // Singleton tuple
        let singleton_tuple = NetType::Tuple(vec![NetType::Int]);
        let res_single = decode_type(singleton_tuple);
        assert!(res_single.is_err());
        assert!(matches!(res_single.unwrap_err(), Error::Message(_)));
    }

    /// Verify round-trip encoding and decoding for a valid tuple type
    #[test]
    fn test_codec_tuple_valid_roundtrip() {
        let original_type = Type::Tuple(TupleType::new(vec![Type::Int, Type::String]).unwrap());

        let encoded_type = encode_type(&original_type).unwrap();
        assert_eq!(
            encoded_type,
            NetType::Tuple(vec![NetType::Int, NetType::String])
        );

        let decoded_type = decode_type(encoded_type).unwrap();
        assert_eq!(original_type, decoded_type);
    }
}

#[cfg(test)]
mod service_code_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Create a unique temp directory for a test, so tests do not share
    /// filesystem state (which would make them order-dependent or flaky).
    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "meerkat_served_{}_{}_{}_{}",
            label,
            std::process::id(),
            nanos,
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #39: a valid relative path within the base directory resolves.
    #[test]
    fn test_resolve_served_file_valid() {
        let dir = unique_temp_dir("valid");
        let file = dir.join("counter.mkt");
        std::fs::write(&file, "service counter {}").unwrap();

        let resolved = resolve_served_file(&dir, "counter.mkt").expect("should resolve");
        assert_eq!(resolved, file.canonicalize().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #39: an absolute path is rejected.
    #[test]
    fn test_resolve_served_file_rejects_absolute() {
        let dir = unique_temp_dir("abs");
        let res = resolve_served_file(&dir, "/etc/passwd");
        assert!(res.is_err(), "absolute path must be rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #39: a traversal path escaping the base directory is rejected.
    #[test]
    fn test_resolve_served_file_rejects_traversal() {
        let root = unique_temp_dir("trav");
        let base = root.join("base");
        std::fs::create_dir_all(&base).unwrap();
        // A secret file outside the base directory but inside the unique root.
        std::fs::write(root.join("secret.mkt"), "secret").unwrap();

        let res = resolve_served_file(&base, "../secret.mkt");
        assert!(res.is_err(), "traversal outside base must be rejected");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// #39: a missing file is rejected (not found).
    #[test]
    fn test_resolve_served_file_missing() {
        let dir = unique_temp_dir("missing");
        let res = resolve_served_file(&dir, "nope.mkt");
        assert!(res.is_err(), "missing file must be rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Verify round-trip encoding and decoding for a ServiceType
    #[test]
    fn test_codec_service_type_roundtrip() {
        let mut interner = Interner::new();
        let field_a = interner.insert("a");
        let field_b = interner.insert("b");

        let mut original_type = ServiceType::default();
        original_type.add_field(field_a, Type::Int).unwrap();
        original_type.add_field(field_b, Type::String).unwrap();

        let encoded = encode_servicetype(&original_type, &interner).unwrap();

        let mut interner_new = Interner::new();
        let decoded = decode_servicetype(encoded.clone(), &mut interner_new).unwrap();

        // Note: We re-encode to compare `NetServiceType` values
        // instead of comparing the decoded runtime `ServiceType`
        // instances directly. Runtime `ServiceType` equality
        // depends on `Interner` `Symbol` IDs, which might
        // misalign between the original and new `Interner`
        // instances, causing brittle test failures. The
        // `NetServiceType` correctly reflects network equivalence
        let reencoded = encode_servicetype(&decoded, &interner_new).unwrap();
        assert_eq!(encoded, reencoded);
    }

    /// Verify round-trip encoding and decoding for an empty ServiceType (zero fields)
    #[test]
    fn test_codec_service_type_empty_roundtrip() {
        let interner = Interner::new();
        let original_type = ServiceType::default();

        let encoded = encode_servicetype(&original_type, &interner).unwrap();
        assert!(encoded.fields.is_empty());

        let mut interner_new = Interner::new();
        let decoded = decode_servicetype(encoded.clone(), &mut interner_new).unwrap();

        // Note: We re-encode to compare `NetServiceType` values
        // instead of comparing the decoded runtime `ServiceType`
        // instances directly. Runtime `ServiceType` equality
        // depends on `Interner` `Symbol` IDs, which might
        // misalign between the original and new `Interner`
        // instances, causing brittle test failures. The
        // `NetServiceType` correctly reflects network equivalence
        let reencoded = encode_servicetype(&decoded, &interner_new).unwrap();
        assert_eq!(encoded, reencoded);
    }

    /// Verify round-trip encoding and decoding for a ServiceType with complex types (Tuple, Func)
    #[test]
    fn test_codec_service_type_complex_roundtrip() {
        let mut interner = Interner::new();
        let field_tuple = interner.insert("t");
        let field_func = interner.insert("f");

        let tuple_ty = Type::Tuple(TupleType::new(vec![Type::Int, Type::String]).unwrap());
        let func_ty = Type::Func(
            Box::new(Type::Bool),
            Box::new(Type::Unit),
            std::collections::HashSet::new(),
        );

        let mut original_type = ServiceType::default();
        original_type.add_field(field_tuple, tuple_ty).unwrap();
        original_type.add_field(field_func, func_ty).unwrap();

        let encoded = encode_servicetype(&original_type, &interner).unwrap();

        let mut interner_new = Interner::new();
        let decoded = decode_servicetype(encoded.clone(), &mut interner_new).unwrap();

        // Note: We re-encode to compare `NetServiceType` values
        // instead of comparing the decoded runtime `ServiceType`
        // instances directly. Runtime `ServiceType` equality
        // depends on `Interner` `Symbol` IDs, which might
        // misalign between the original and new `Interner`
        // instances, causing brittle test failures. The
        // `NetServiceType` correctly reflects network equivalence
        let reencoded = encode_servicetype(&decoded, &interner_new).unwrap();
        assert_eq!(encoded, reencoded);
    }

    /// Verify that decode_servicetype returns an error if NetServiceType has duplicate field names
    #[test]
    fn test_codec_service_type_duplicate_fields_error() {
        let invalid_nst = NetServiceType {
            fields: vec![
                ("duplicate_field".to_string(), NetType::Int),
                ("duplicate_field".to_string(), NetType::String),
            ],
        };

        let mut interner = Interner::new();
        let res = decode_servicetype(invalid_nst, &mut interner);
        assert!(res.is_err());
        assert!(
            matches!(res.unwrap_err(), Error::Message(ref msg) if msg.contains("duplicate field name"))
        );
    }

    /// Verify that validate_lookup_request accepts valid identifiers and rejects invalid ones
    #[test]
    fn test_validate_lookup_request() {
        assert!(validate_lookup_request("valid_service", "valid_member").is_ok());
        assert!(validate_lookup_request("invalid-service!", "valid_member").is_err());
        assert!(validate_lookup_request("valid_service", "123invalid_member").is_err());

        let overlong_str = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        assert!(validate_lookup_request(&overlong_str, "valid_member").is_err());
    }

    /// Verify that validate_action_request accepts valid identifiers and rejects invalid ones
    #[test]
    fn test_validate_action_request() {
        assert!(validate_action_request("valid_service").is_ok());
        assert!(validate_action_request("invalid service").is_err());

        let overlong_str = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        assert!(validate_action_request(&overlong_str).is_err());
    }

    /// Verify that validate_lock_request accepts valid lock groups and rejects invalid service or member identifiers
    #[test]
    fn test_validate_lock_request() {
        use std::collections::HashSet;

        let mut services = HashMap::new();
        let mut reads = HashSet::new();
        reads.insert("read_var".to_string());
        let mut writes = HashSet::new();
        writes.insert("write_var".to_string());

        services.insert(
            "valid_service".to_string(),
            LockGroup {
                service_level_lock: false,
                reads,
                writes,
            },
        );

        assert!(validate_lock_request(&services).is_ok());

        let mut invalid_svc_map = HashMap::new();
        invalid_svc_map.insert(
            "bad-service-name!".to_string(),
            LockGroup {
                service_level_lock: false,
                reads: HashSet::new(),
                writes: HashSet::new(),
            },
        );
        assert!(validate_lock_request(&invalid_svc_map).is_err());

        let mut invalid_read_map = HashMap::new();
        let mut bad_reads = HashSet::new();
        bad_reads.insert("bad member name!".to_string());
        invalid_read_map.insert(
            "valid_service".to_string(),
            LockGroup {
                service_level_lock: false,
                reads: bad_reads,
                writes: HashSet::new(),
            },
        );
        assert!(validate_lock_request(&invalid_read_map).is_err());
    }

    /// Verify that `decode_update_service_request` validates service identifiers
    /// and enforces maximum source length limits
    #[test]
    fn test_decode_update_service_request() {
        let mut interner = Interner::new();

        let valid_res = decode_update_service_request(
            "MyService",
            "update MyService { var x = 1; }",
            &mut interner,
        );
        assert!(valid_res.is_ok());
        let (sym, src) = valid_res.unwrap();
        assert_eq!(interner.get(sym), "MyService");
        assert_eq!(src, "update MyService { var x = 1; }");

        let invalid_svc = decode_update_service_request(
            "invalid-service!",
            "update invalid-service! {}",
            &mut interner,
        );
        assert!(invalid_svc.is_err());

        let oversized_source = "a".repeat(MAX_NET_REQUEST_SOURCE_LENGTH + 1);
        let oversized_res =
            decode_update_service_request("MyService", &oversized_source, &mut interner);
        assert!(oversized_res.is_err());
    }
}
