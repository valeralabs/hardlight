use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput};

#[proc_macro_attribute]
pub fn connection_state(_attr: TokenStream, input: TokenStream) -> TokenStream {
    let input_ast = parse_macro_input!(input as DeriveInput);
    let state_ident = &input_ast.ident;
    let (_, ty_generics, where_clause) = input_ast.generics.split_for_impl();

    let field_names: Vec<_> = match input_ast.data {
        syn::Data::Struct(ref data_struct) => data_struct
            .fields
            .iter()
            .map(|f| f.ident.as_ref().unwrap())
            .collect(),
        _ => panic!("ConnectionState can only be derived on structs"),
    };
    
    let field_strings: Vec<_> = field_names
        .iter()
        .map(|name| name.to_string())
        .collect();

    let expanded = quote! {
        #[derive(Clone, Default, Debug)]
        #input_ast

        struct StateController {
            state: std::sync::Mutex<#state_ident>,
            channel: std::sync::Arc<tokio::sync::mpsc::Sender<Vec<(String, Vec<u8>)>>>,
        }

        impl StateController {
            fn new(channel: StateUpdateChannel) -> Self {
                Self {
                    state: std::sync::Mutex::new(Default::default()),
                    channel: std::sync::Arc::new(channel),
                }
            }

            fn lock(&self) -> StateGuard {
                let state = self.state.lock().unwrap();
                StateGuard {
                    starting_state: state.clone(),
                    state,
                    channel: self.channel.clone(),
                }
            }
        }

        struct StateGuard<'a> {
            state: std::sync::MutexGuard<'a, #state_ident>,
            starting_state: #state_ident,
            channel: std::sync::Arc<tokio::sync::mpsc::Sender<Vec<(String, Vec<u8>)>>>,
        }

        impl<'a> Drop for StateGuard<'a> {
            fn drop(&mut self) {
                let mut changes = Vec::new();

                #(
                    if self.state.#field_names != self.starting_state.#field_names {
                        changes.push((
                            #field_strings.to_string(),
                            rkyv::to_bytes::<_, 1024>(&self.state.#field_names).unwrap().to_vec(),
                        ));
                    }
                )*

                if changes.is_empty() {
                    return;
                }

                let channel = self.channel.clone();
                tokio::spawn(async move {
                    let _ = channel.send(changes).await;
                });
            }
        }

        impl std::ops::Deref for StateGuard<'_> {
            type Target = #state_ident;

            fn deref(&self) -> &Self::Target {
                &self.state
            }
        }

        impl std::ops::DerefMut for StateGuard<'_> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.state
            }
        }

        impl ClientState for #state_ident #ty_generics #where_clause {
            fn apply_changes(
                &mut self,
                changes: Vec<(String, Vec<u8>)>,
            ) -> HandlerResult<()> {
                for (field, new_value) in changes {
                    match field.as_ref() {
                        #(
                            #field_strings => {
                                self.#field_names = rkyv::from_bytes(&new_value)
                                    .map_err(|_| RpcHandlerError::BadInputBytes)?;
                            }
                        ),*
                        _ => {}
                    }
                }
                Ok(())
            }
        }
    };

    TokenStream::from(expanded)
}
