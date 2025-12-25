use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse::Parser, parse_macro_input, punctuated::Punctuated, Expr, ExprLit, ItemFn, Lit, Meta,
};

#[proc_macro_attribute]
pub fn jni(args: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let name = func.sig.ident.to_string();
    
    let use_consts = args.is_empty();
    
    let attrs = &func.attrs;
    let vis   = &func.vis;
    let sig   = &func.sig;
    let block = &func.block;

    if use_consts {
        // Use constants from jni_config!
        quote! {
            #[no_mangle]
            #[export_name = concat!("Java_", __JNI_BASE, "_", __JNI_CLASS, "_", #name)]
            #[allow(non_snake_case)]
            #(#attrs)*
            #vis #sig #block
        }.into()
    } else {
        let args = Punctuated::<Meta, syn::Token![,]>::parse_terminated
            .parse(args)
            .expect("Failed to parse attributes");

        let mut base = String::new();
        let mut class = String::new();

        for arg in args {
            if let Meta::NameValue(nv) = arg {
                if nv.path.is_ident("base") {
                    if let Expr::Lit(ExprLit {
                        lit: Lit::Str(ref s),
                        ..
                    }) = nv.value
                    {
                        base = s.value();
                    }
                }
                if nv.path.is_ident("class") {
                    if let Expr::Lit(ExprLit {
                        lit: Lit::Str(ref s),
                        ..
                    }) = nv.value
                    {
                        class = s.value();
                    }
                }
            }
        }

        let jni_name = format!("Java_{}_{}_{}", base, class, name)
            .replace('.', "_");

        quote! {
            #[no_mangle]
            #[allow(non_snake_case)]
            #[export_name = #jni_name]
            #(#attrs)*
            #vis #sig #block
        }
        .into()
    }
}
