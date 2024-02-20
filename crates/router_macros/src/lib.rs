use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use syn::{
    parse::Parse, parse_macro_input, Attribute, Data, DeriveInput, Fields, FieldsNamed,
    FieldsUnnamed, Ident, ItemEnum, LitStr, Result, Type, Variant,
};

struct Args {
    state: Option<Type>,
}

impl Parse for Args {
    fn parse(input: syn::parse::ParseStream) -> Result<Self> {
        let state = input.parse::<Type>().ok();

        Ok(Self { state })
    }
}

#[proc_macro_attribute]
pub fn router(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as Args);
    let input = parse_macro_input!(input as ItemEnum);
    match router_macro(args, input) {
        Ok(s) => s.to_token_stream().into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn router_macro(args: Args, item_enum: ItemEnum) -> Result<TokenStream2> {
    let attr = match args.state {
        Some(st) => quote! { #st },
        None => quote! { () },
    };

    let expanded = quote! {
        #[derive(router::Routes)]
        #[state(#attr)]
        #item_enum
    };

    Ok(expanded)
}

#[proc_macro_derive(
    Routes,
    attributes(get, post, delete, patch, put, state, embed, folder)
)]
pub fn routes(s: TokenStream) -> TokenStream {
    let input = parse_macro_input!(s as DeriveInput);
    match routes_macro(input) {
        Ok(s) => s.to_token_stream().into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn routes_macro(input: DeriveInput) -> Result<TokenStream2> {
    let enum_name = input.ident;
    let Data::Enum(data) = input.data else {
        panic!("Only enums are supported");
    };

    let arg = input
        .attrs
        .iter()
        .filter(|attr| match attr.path.get_ident() {
            Some(ident) => ident.to_string() == "state",
            None => false,
        })
        .filter_map(args)
        .last();

    let state_generic = match arg {
        Some(Args { state }) => quote! { #state },
        None => quote! { () },
    };

    let variants = data
        .variants
        .iter()
        .map(|variant| RouteVariant::from(variant))
        .collect::<Vec<_>>();

    let urls = variants
        .iter()
        .map(
            |RouteVariant {
                 ref variant,
                 ref fields,
                 ref path,
                 ..
             }| {
                let left = left(&enum_name, variant, fields);
                let right = right(fields, path);

                quote! { #left => #right }
            },
        )
        .collect::<Vec<_>>();

    let methods = variants
        .iter()
        .map(
            |RouteVariant {
                 ref method,
                 ref variant,
                 ref fields,
                 ..
             }| {
                let left = left(&enum_name, variant, fields);
                let right = method.to_string();

                quote! { #left => #right.to_owned() }
            },
        )
        .collect::<Vec<_>>();

    let axum_route = variants
        .iter()
        .map(
            |RouteVariant {
                 ref path,
                 ref method,
                 ref variant,
                 ..
             }| {
                let fn_string = pascal_to_camel(&variant.to_string());
                let fn_name = Ident::new(&fn_string, method.span());
                quote! { .route(#path, #method(#fn_name)) }
            },
        )
        .collect::<Vec<_>>();

    let embed_attr = data
        .variants
        .iter()
        .filter(|variant| {
            variant
                .attrs
                .iter()
                .find(|attr| attr.path.is_ident("embed"))
                .is_some()
        })
        .last();
    let embed_ident = match embed_attr {
        Some(variant) => Some(&variant.ident),
        None => None,
    };
    let folder_attr = data
        .variants
        .iter()
        .filter_map(|variant| {
            variant
                .attrs
                .iter()
                .find(|attr| attr.path.is_ident("folder"))
        })
        .last();
    let folder = match folder_attr {
        Some(attr) => match attr.parse_args::<LitStr>() {
            Ok(lit_str) => lit_str.value(),
            Err(_) => "static".into(),
        },
        None => "static".into(),
    };
    let static_file_handler = if let Some(ident) = embed_ident {
        let fn_name = Ident::new(&pascal_to_camel(&ident.to_string()), ident.span());
        quote! {
            async fn #fn_name(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
                match #ident::get(uri.path()) {
                    Some((content_type, bytes)) => (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        bytes,
                    ),
                    None => (
                        axum::http::StatusCode::NOT_FOUND,
                        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        "not found".as_bytes(),
                    ),
                }
            }

            #[derive(static_files::StaticFiles)]
            #[folder(#folder)]
            struct #ident;
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        impl #enum_name {
            fn url(&self) -> String {
                match self {
                    #(#urls,)*
                }
            }

            #[allow(unused)]
            fn method(&self) -> String {
                match self {
                    #(#methods,)*
                }
            }

            fn router() -> ::axum::Router<#state_generic> {
                use ::axum::routing::{get, post, patch, put, delete};
                ::axum::Router::new()#(#axum_route)*
            }
        }

        impl std::fmt::Display for #enum_name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_fmt(format_args!("{}", self.url()))
            }
        }

        #static_file_handler
    })
}

fn right_from_unnamed(path: &LitStr, fields: &FieldsUnnamed) -> TokenStream2 {
    let format = path
        .value()
        .split('/')
        .map(|part| if part.starts_with(":") { "{}" } else { part })
        .collect::<Vec<_>>()
        .join("/");

    let idents = fields
        .unnamed
        .iter()
        .enumerate()
        .map(|(i, _field)| Ident::new(&format!("x{}", i), Span::call_site()))
        .collect::<Vec<_>>();

    quote! { format!(#format, #(#idents,)*) }
}

fn right_from_named(fields: &FieldsNamed, path: &LitStr) -> TokenStream2 {
    let idents = fields
        .named
        .iter()
        .map(|field| field.ident.as_ref().unwrap())
        .collect::<Vec<_>>();

    let query = idents
        .iter()
        .map(|ident| format!("{}={{:?}}", ident))
        .collect::<Vec<_>>()
        .join("&");

    let format = format!("{}?{}", path.value(), query);

    quote! { format!(#format, #(#idents,)*) }
}

fn right(fields: &Fields, path: &LitStr) -> TokenStream2 {
    match fields {
        Fields::Named(fields) => right_from_named(fields, path),
        Fields::Unnamed(fields) => right_from_unnamed(path, fields),
        Fields::Unit => quote! { #path.to_owned() },
    }
}

fn left_from_named(r#ident: &Ident, variant: &Ident, fields: &FieldsNamed) -> TokenStream2 {
    let idents = fields
        .named
        .iter()
        .map(|field| field.ident.as_ref().unwrap())
        .collect::<Vec<_>>();

    quote! {
        #r#ident::#variant { #(#idents,)* }
    }
}

fn left_from_unnamed(r#ident: &Ident, variant: &Ident, fields: &FieldsUnnamed) -> TokenStream2 {
    let idents = fields
        .unnamed
        .iter()
        .enumerate()
        .map(|(i, _field)| Ident::new(&format!("x{}", i), Span::call_site()))
        .collect::<Vec<_>>();

    quote! {
        #r#ident::#variant(#(#idents,)*)
    }
}

fn left(r#ident: &Ident, variant: &Ident, fields: &Fields) -> TokenStream2 {
    match fields {
        syn::Fields::Named(fields) => left_from_named(r#ident, variant, fields),
        syn::Fields::Unnamed(fields) => left_from_unnamed(r#ident, variant, fields),
        syn::Fields::Unit => quote! { #r#ident::#variant },
    }
}

fn pascal_to_camel(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars();
    if let Some(char) = &chars.nth(0) {
        result.push(char.to_ascii_lowercase());
    }

    while let Some(char) = chars.next() {
        if char.is_uppercase() {
            result.push('_');
            result.push(char.to_lowercase().next().unwrap());
        } else {
            result.push(char);
        }
    }

    result
}

#[derive(Clone)]
struct RouteVariant<'a> {
    method: Ident,
    path: LitStr,
    variant: &'a Ident,
    fields: &'a Fields,
}

impl<'a> From<&'a Variant> for RouteVariant<'a> {
    fn from(value: &'a Variant) -> Self {
        let variant = &value.ident;
        let (method, path) = value
            .attrs
            .iter()
            .filter_map(
                |attr| match (attr.path.get_ident(), attr.parse_args::<LitStr>().ok()) {
                    (Some(ident), Some(path)) => {
                        if ident.to_string() != "folder" {
                            Some((ident.clone(), path))
                        } else {
                            None
                        }
                    }
                    (Some(ident), None) => {
                        // HACK: assume this is #[embed]
                        Some((
                            Ident::new("get", ident.span()),
                            LitStr::new("/*file", ident.span()),
                        ))
                    }
                    _ => None,
                },
            )
            .last()
            .expect(
                "should be #[get], #[post], #[put], #[delete], #[state], #[embed] or #[folder]",
            );
        let fields = &value.fields;

        RouteVariant {
            path,
            method,
            variant,
            fields,
        }
    }
}

fn args(attr: &Attribute) -> Option<Args> {
    attr.parse_args::<Args>().ok()
}