//! Read a Move function's signature off the chain that runs it.
//!
//! # This is what replaces an SDK
//!
//! Integrating a protocol needs one thing: the exact shape of the call. TypeScript gets that from a
//! per-protocol SDK, which is why "does it have an SDK?" is the first question asked there — and why
//! the answer decides which protocols are reachable.
//!
//! But an SDK is a hand-maintained copy of something the chain already publishes, and a copy can be
//! stale. `@mysten/deepbook-v3` converts a price through a double before sending it; a signer in the
//! reference implementation required an entry point its deployed contract no longer had. Both are
//! failures of the copy, not of the contract.
//!
//! The chain's own answer cannot drift from the contract, because it *is* the contract. So the
//! question "does Rust support protocol X?" has the same answer for every X: the package is
//! deployed, so its signature is readable, so the call can be built. No SDK is involved and none is
//! waited for.
//!
//! What is left is arranging arguments in the order the descriptor gives — which is what
//! `rill-ptb` does, and what `tests/deepbook_signature.rs` checks it still does.

use sui_rpc::proto::sui::rpc::v2::{
    open_signature, open_signature_body, GetFunctionRequest, OpenSignature, OpenSignatureBody,
};

use crate::{ChainError, ChainResult};

/// One parameter, as the deployed package declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// `&`, `&mut`, or nothing — what the call must hand over.
    pub reference: &'static str,
    /// The type, rendered the way it is written in Move.
    pub type_name: String,
}

impl Parameter {
    /// The runtime supplies `TxContext`; it is a parameter of the function but never an argument of
    /// the command. Counting it makes a correct call look one argument short.
    pub fn is_tx_context(&self) -> bool {
        self.type_name.ends_with("::tx_context::TxContext")
    }
}

impl std::fmt::Display for Parameter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.reference, self.type_name)
    }
}

/// A function, as the deployed package declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSignature {
    pub module: String,
    pub name: String,
    pub type_parameter_count: usize,
    pub parameters: Vec<Parameter>,
    pub returns: Vec<Parameter>,
    pub is_entry: bool,
}

impl FunctionSignature {
    /// The arguments a PTB command must carry, in order — `TxContext` excluded.
    pub fn call_arguments(&self) -> Vec<&Parameter> {
        self.parameters
            .iter()
            .filter(|p| !p.is_tx_context())
            .collect()
    }

    /// How many arguments a builder must emit for this call.
    pub fn arity(&self) -> usize {
        self.call_arguments().len()
    }
}

impl std::fmt::Display for FunctionSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let generics = if self.type_parameter_count > 0 {
            let names: Vec<String> = (0..self.type_parameter_count)
                .map(|i| format!("T{i}"))
                .collect();
            format!("<{}>", names.join(", "))
        } else {
            String::new()
        };
        let params: Vec<String> = self.parameters.iter().map(ToString::to_string).collect();
        write!(
            f,
            "public {}fun {}::{}{generics}({})",
            if self.is_entry { "entry " } else { "" },
            self.module,
            self.name,
            params.join(", ")
        )?;
        if !self.returns.is_empty() {
            let returns: Vec<String> = self.returns.iter().map(ToString::to_string).collect();
            write!(f, ": {}", returns.join(", "))?;
        }
        Ok(())
    }
}

fn render_body(body: Option<&OpenSignatureBody>) -> String {
    let Some(body) = body else {
        return "?".into();
    };
    use open_signature_body::Type;
    match body.r#type() {
        Type::Address => "address".into(),
        Type::Bool => "bool".into(),
        Type::U8 => "u8".into(),
        Type::U16 => "u16".into(),
        Type::U32 => "u32".into(),
        Type::U64 => "u64".into(),
        Type::U128 => "u128".into(),
        Type::U256 => "u256".into(),
        Type::Vector => format!(
            "vector<{}>",
            render_body(body.type_parameter_instantiation.first())
        ),
        Type::Datatype => {
            let name = body.type_name.clone().unwrap_or_else(|| "?".into());
            if body.type_parameter_instantiation.is_empty() {
                name
            } else {
                let args: Vec<String> = body
                    .type_parameter_instantiation
                    .iter()
                    .map(|t| render_body(Some(t)))
                    .collect();
                format!("{name}<{}>", args.join(", "))
            }
        }
        // A generic slot, written the way the function declares it. `Type` is non-exhaustive, so
        // anything new renders as a placeholder rather than failing to compile against a newer
        // node — an unrecognised type is worth showing, not worth refusing to show the rest for.
        _ => match body.type_parameter {
            Some(index) => format!("T{index}"),
            None => "?".into(),
        },
    }
}

fn to_parameter(signature: &OpenSignature) -> Parameter {
    Parameter {
        reference: match signature.reference() {
            open_signature::Reference::Mutable => "&mut ",
            open_signature::Reference::Immutable => "&",
            // Non-exhaustive upstream; anything else means by-value.
            _ => "",
        },
        type_name: render_body(signature.body.as_ref()),
    }
}

/// Ask a deployed package what one of its functions takes.
pub async fn describe_function(
    endpoint: &str,
    package_id: &str,
    module: &str,
    function: &str,
) -> ChainResult<FunctionSignature> {
    let client =
        sui_rpc::client::Client::new(endpoint).map_err(|e| ChainError::Transport(e.to_string()))?;

    let mut request = GetFunctionRequest::default();
    request.package_id = Some(package_id.to_owned());
    request.module_name = Some(module.to_owned());
    request.name = Some(function.to_owned());

    let response = client
        .clone()
        .package_client()
        .get_function(request)
        .await
        .map_err(|s| match s.code() {
            tonic::Code::NotFound => {
                ChainError::NotFound(format!("{package_id}::{module}::{function}"))
            }
            _ => ChainError::Transport(s.message().to_owned()),
        })?
        .into_inner();

    let descriptor = response
        .function
        .ok_or_else(|| ChainError::NotFound(format!("{package_id}::{module}::{function}")))?;

    Ok(FunctionSignature {
        module: module.to_owned(),
        name: function.to_owned(),
        type_parameter_count: descriptor.type_parameters.len(),
        parameters: descriptor.parameters.iter().map(to_parameter).collect(),
        returns: descriptor.returns.iter().map(to_parameter).collect(),
        is_entry: descriptor.is_entry.unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(reference: &'static str, type_name: &str) -> Parameter {
        Parameter {
            reference,
            type_name: type_name.into(),
        }
    }

    #[test]
    fn tx_context_is_recognised_and_excluded_from_the_arity() {
        let signature = FunctionSignature {
            module: "pool".into(),
            name: "place_limit_order".into(),
            type_parameter_count: 2,
            parameters: vec![
                param("&mut ", "0xdee9::pool::Pool<T0, T1>"),
                param("&mut ", "0xdee9::balance_manager::BalanceManager"),
                param(
                    "&mut ",
                    "0x0000000000000000000000000000000000000000000000000000000000000002::tx_context::TxContext",
                ),
            ],
            returns: vec![],
            is_entry: false,
        };
        assert_eq!(
            signature.arity(),
            2,
            "the runtime supplies TxContext; a builder never passes it"
        );
    }

    #[test]
    fn a_signature_renders_the_way_move_writes_it() {
        let signature = FunctionSignature {
            module: "pool".into(),
            name: "mid_price".into(),
            type_parameter_count: 2,
            parameters: vec![
                param("&", "0xdee9::pool::Pool<T0, T1>"),
                param("&", "0x2::clock::Clock"),
            ],
            returns: vec![param("", "u64")],
            is_entry: false,
        };
        assert_eq!(
            signature.to_string(),
            "public fun pool::mid_price<T0, T1>(&0xdee9::pool::Pool<T0, T1>, &0x2::clock::Clock): u64"
        );
    }

    #[test]
    fn a_function_with_no_generics_renders_without_angle_brackets() {
        let signature = FunctionSignature {
            module: "guard".into(),
            name: "assert_min_value".into(),
            type_parameter_count: 0,
            parameters: vec![param("", "u64")],
            returns: vec![],
            is_entry: false,
        };
        assert!(!signature.to_string().contains('<'));
    }
}
