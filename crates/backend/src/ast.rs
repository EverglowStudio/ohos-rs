use convert_case::Case;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use syn::{Attribute, Expr, Type};

#[derive(Debug, Clone)]
pub struct NapiFn {
  pub name: Ident,
  pub js_name: String,
  pub module_exports: bool,
  pub attrs: Vec<Attribute>,
  pub args: Vec<NapiFnArg>,
  pub ret: Option<syn::Type>,
  pub is_ret_result: bool,
  pub is_async: bool,
  pub within_async_runtime: bool,
  pub fn_self: Option<FnSelf>,
  pub kind: FnKind,
  pub vis: syn::Visibility,
  pub parent: Option<Ident>,
  pub parent_js_name: Option<String>,
  pub strict: bool,
  pub return_if_invalid: bool,
  pub js_mod: Option<String>,
  pub ts_generic_types: Option<String>,
  pub ts_type: Option<String>,
  pub ts_args_type: Option<String>,
  pub ts_return_type: Option<String>,
  pub skip_typescript: bool,
  pub comments: Vec<String>,
  pub parent_is_generator: bool,
  pub parent_is_async_generator: bool,
  pub writable: bool,
  pub enumerable: bool,
  pub configurable: bool,
  pub catch_unwind: bool,
  pub unsafe_: bool,
  pub register_name: Ident,
  pub no_export: bool,
  /// Programmatic conversion performed after N-API values are decoded on the
  /// JavaScript thread and before an async body is moved to the executor.
  ///
  /// This is intentionally backend plumbing rather than metadata discovery.
  /// UniFFI uses it to turn Ark Host-backed callback/stream IDs into Send-safe
  /// Rust proxies without moving an Ark `Object` into a Tokio future.
  pub pre_call: TokenStream,
  /// Number of leading decoded wrapper arguments consumed exclusively by
  /// `pre_call`.  They are deliberately omitted from the Rust function call.
  pub leading_wrapper_args: usize,
}

/// Programmatic construction API for [`NapiFn`].
///
/// UniFFI's OHOS engine consumes a frozen Rust call plan and uses the same
/// backend AST/codegen as the `#[napi]` macro.  This builder intentionally
/// exposes only backend mechanics; it does not discover component metadata or
/// read process configuration.
#[derive(Debug, Clone)]
pub struct NapiFnBuilder {
  function: NapiFn,
}

impl NapiFnBuilder {
  pub fn new(name: Ident, js_name: impl Into<String>) -> Self {
    let register_name = Ident::new(&format!("__napi_register_{}", name), Span::call_site());
    Self {
      function: NapiFn {
        name,
        js_name: js_name.into(),
        module_exports: false,
        attrs: Vec::new(),
        args: Vec::new(),
        ret: None,
        is_ret_result: false,
        is_async: false,
        within_async_runtime: false,
        fn_self: None,
        kind: FnKind::Normal,
        vis: syn::Visibility::Inherited,
        parent: None,
        parent_js_name: None,
        strict: false,
        return_if_invalid: false,
        js_mod: None,
        ts_generic_types: None,
        ts_type: None,
        ts_args_type: None,
        ts_return_type: None,
        skip_typescript: false,
        comments: Vec::new(),
        parent_is_generator: false,
        parent_is_async_generator: false,
        writable: true,
        enumerable: true,
        configurable: true,
        catch_unwind: false,
        unsafe_: false,
        register_name,
        no_export: false,
        pre_call: TokenStream::new(),
        leading_wrapper_args: 0,
      },
    }
  }

  pub fn argument(mut self, argument: NapiFnArg) -> Self {
    self.function.args.push(argument);
    self
  }

  pub fn return_type(mut self, return_type: Type) -> Self {
    self.function.ret = Some(return_type);
    self.function.is_ret_result = false;
    self
  }

  pub fn result_return_type(mut self, return_type: Type) -> Self {
    self.function.ret = Some(return_type);
    self.function.is_ret_result = true;
    self
  }

  pub fn asynchronous(mut self, asynchronous: bool) -> Self {
    self.function.is_async = asynchronous;
    self
  }

  pub fn within_async_runtime(mut self, within_async_runtime: bool) -> Self {
    self.function.within_async_runtime = within_async_runtime;
    self
  }

  pub fn strict(mut self, strict: bool) -> Self {
    self.function.strict = strict;
    self
  }

  /// Register this function through the module-init hook instead of the
  /// ordinary property export path.  This is reserved for true module
  /// initializers; the UniFFI engine factory is a normal single property
  /// export and raw operation callbacks remain `private(true)`.
  pub fn module_exports(mut self, module_exports: bool) -> Self {
    self.function.module_exports = module_exports;
    self
  }

  pub fn skip_typescript(mut self, skip_typescript: bool) -> Self {
    self.function.skip_typescript = skip_typescript;
    self
  }

  /// Keep the callback available to sibling generated code but do not expose
  /// it through the module registration table.
  pub fn private(mut self, private: bool) -> Self {
    self.function.no_export = private;
    self
  }

  pub fn register_name(mut self, register_name: Ident) -> Self {
    self.function.register_name = register_name;
    self
  }

  pub fn pre_call(mut self, pre_call: TokenStream) -> Self {
    self.function.pre_call = pre_call;
    self
  }

  pub fn leading_wrapper_args(mut self, count: usize) -> Self {
    self.function.leading_wrapper_args = count;
    self
  }

  pub fn build(self) -> NapiFn {
    self.function
  }
}

#[derive(Debug, Clone)]
pub struct CallbackArg {
  pub pat: Box<syn::Pat>,
  pub args: Vec<syn::Type>,
  pub ret: Option<syn::Type>,
}

#[derive(Debug, Clone)]
pub struct NapiFnArg {
  pub kind: NapiFnArgKind,
  pub ts_arg_type: Option<String>,
}

impl NapiFnArg {
  /// if type was overridden with `#[napi(ts_arg_type = "...")]` use that instead
  pub fn use_overridden_type_or(&self, default: impl FnOnce() -> String) -> String {
    self.ts_arg_type.as_ref().cloned().unwrap_or_else(default)
  }
}

#[derive(Debug, Clone)]
pub enum NapiFnArgKind {
  PatType(Box<syn::PatType>),
  Callback(Box<CallbackArg>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnKind {
  Normal,
  Constructor,
  Factory,
  Getter,
  Setter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnSelf {
  Value,
  Ref,
  MutRef,
}

#[derive(Debug, Clone)]
pub struct NapiStruct {
  pub name: Ident,
  pub js_name: String,
  pub comments: Vec<String>,
  pub js_mod: Option<String>,
  pub use_nullable: bool,
  pub register_name: Ident,
  pub kind: NapiStructKind,
  pub has_lifetime: bool,
  pub is_generator: bool,
  pub is_async_generator: bool,
}

#[derive(Debug, Clone)]
pub enum NapiStructKind {
  Transparent(NapiTransparent),
  Class(NapiClass),
  Object(NapiObject),
  StructuredEnum(NapiStructuredEnum),
  Array(NapiArray),
}

#[derive(Debug, Clone)]
pub struct NapiTransparent {
  pub ty: Type,
  pub object_from_js: bool,
  pub object_to_js: bool,
}

#[derive(Debug, Clone)]
pub struct NapiClass {
  pub fields: Vec<NapiStructField>,
  pub ctor: bool,
  pub implement_iterator: bool,
  pub implement_async_iterator: bool,
  pub is_tuple: bool,
  pub use_custom_finalize: bool,
}

#[derive(Debug, Clone)]
pub struct NapiObject {
  pub fields: Vec<NapiStructField>,
  pub object_from_js: bool,
  pub object_to_js: bool,
  pub is_tuple: bool,
}

#[derive(Debug, Clone)]
pub struct NapiArray {
  pub fields: Vec<NapiStructField>,
  pub object_from_js: bool,
  pub object_to_js: bool,
}

#[derive(Debug, Clone)]
pub struct NapiStructuredEnum {
  pub variants: Vec<NapiStructuredEnumVariant>,
  pub object_from_js: bool,
  pub object_to_js: bool,
  pub discriminant: String,
  pub discriminant_case: Option<Case<'static>>,
}

#[derive(Debug, Clone)]
pub struct NapiStructuredEnumVariant {
  pub name: Ident,
  pub fields: Vec<NapiStructField>,
  pub is_tuple: bool,
}

#[derive(Debug, Clone)]
pub struct NapiStructField {
  pub name: syn::Member,
  pub js_name: String,
  pub ty: syn::Type,
  pub getter: bool,
  pub setter: bool,
  pub writable: bool,
  pub enumerable: bool,
  pub configurable: bool,
  pub comments: Vec<String>,
  pub skip_typescript: bool,
  pub ts_type: Option<String>,
  pub has_lifetime: bool,
}

#[derive(Debug, Clone)]
pub struct NapiImpl {
  pub name: Ident,
  pub js_name: String,
  pub has_lifetime: bool,
  pub items: Vec<NapiFn>,
  pub task_output_type: Option<Type>,
  pub iterator_yield_type: Option<Type>,
  pub iterator_next_type: Option<Type>,
  pub iterator_return_type: Option<Type>,
  pub async_iterator_yield_type: Option<Type>,
  pub async_iterator_next_type: Option<Type>,
  pub async_iterator_return_type: Option<Type>,
  pub js_mod: Option<String>,
  pub comments: Vec<String>,
  pub register_name: Ident,
}

#[derive(Debug, Clone)]
pub struct NapiEnum {
  pub name: Ident,
  pub js_name: String,
  pub variants: Vec<NapiEnumVariant>,
  pub js_mod: Option<String>,
  pub comments: Vec<String>,
  pub skip_typescript: bool,
  pub register_name: Ident,
  pub is_string_enum: bool,
  pub object_from_js: bool,
  pub object_to_js: bool,
}

#[derive(Debug, Clone)]
pub enum NapiEnumValue {
  String(String),
  Number(i32),
}

impl From<&NapiEnumValue> for Literal {
  fn from(val: &NapiEnumValue) -> Self {
    match val {
      NapiEnumValue::String(string) => Literal::string(string),
      NapiEnumValue::Number(number) => Literal::i32_unsuffixed(number.to_owned()),
    }
  }
}

#[derive(Debug, Clone)]
pub struct NapiEnumVariant {
  pub name: Ident,
  pub val: NapiEnumValue,
  pub comments: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NapiConst {
  pub name: Ident,
  pub js_name: String,
  pub type_name: Type,
  pub value: Expr,
  pub js_mod: Option<String>,
  pub comments: Vec<String>,
  pub skip_typescript: bool,
  pub register_name: Ident,
}

#[derive(Debug, Clone)]
pub struct NapiMod {
  pub name: Ident,
  pub js_name: String,
}

#[derive(Debug, Clone)]
pub struct NapiType {
  pub name: Ident,
  pub js_name: String,
  pub value: Type,
  pub register_name: Ident,
  pub skip_typescript: bool,
  pub js_mod: Option<String>,
  pub comments: Vec<String>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::TryToTokens;
  use proc_macro2::TokenStream;

  #[test]
  fn programmatic_builder_keeps_private_and_factory_registration_distinct() {
    let private = NapiFnBuilder::new(Ident::new("raw", Span::call_site()), "raw")
      .return_type(syn::parse_quote!(()))
      .private(true)
      .build();
    let mut private_tokens = TokenStream::new();
    private.try_to_tokens(&mut private_tokens).unwrap();
    let private_source = private_tokens.to_string();
    assert!(!private_source.contains("register_module_export"));
    assert!(private.no_export);

    let factory = NapiFnBuilder::new(Ident::new("factory", Span::call_site()), "factory").build();
    let mut factory_tokens = TokenStream::new();
    factory.try_to_tokens(&mut factory_tokens).unwrap();
    assert!(!factory.module_exports);
    assert!(!factory.no_export);
    assert!(!factory_tokens
      .to_string()
      .contains("register_module_export_hook"));
    assert!(factory_tokens.to_string().contains("fn factory"));

    let prepared = NapiFnBuilder::new(Ident::new("prepared", Span::call_site()), "prepared")
      .argument(NapiFnArg {
        kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(value: u32))),
        ts_arg_type: None,
      })
      .pre_call(quote::quote!(let arg0 = arg0 + 1;))
      .return_type(syn::parse_quote!(u32))
      .build();
    let mut prepared_tokens = TokenStream::new();
    prepared.try_to_tokens(&mut prepared_tokens).unwrap();
    let prepared_source = prepared_tokens.to_string();
    assert!(prepared_source.contains("let arg0 = arg0 + 1"));
    assert!(
      prepared_source.find("let arg0 = arg0 + 1").unwrap()
        < prepared_source.find("prepared (arg0)").unwrap()
    );

    let wrapper_only = NapiFnBuilder::new(Ident::new("lowered", Span::call_site()), "lowered")
      .argument(NapiFnArg {
        kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(host: u32))),
        ts_arg_type: None,
      })
      .argument(NapiFnArg {
        kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(value: u32))),
        ts_arg_type: None,
      })
      .pre_call(quote::quote!(let arg1 = arg0 + arg1;))
      .leading_wrapper_args(1)
      .return_type(syn::parse_quote!(u32))
      .build();
    let mut wrapper_only_tokens = TokenStream::new();
    wrapper_only
      .try_to_tokens(&mut wrapper_only_tokens)
      .unwrap();
    let wrapper_only_source = wrapper_only_tokens.to_string();
    assert!(wrapper_only_source.contains("let arg1 = arg0 + arg1"));
    assert!(wrapper_only_source.contains("lowered (arg1)"));
    assert!(!wrapper_only_source.contains("lowered (arg0 , arg1)"));
  }
}
