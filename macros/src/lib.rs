use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput};

#[proc_macro_attribute]
pub fn connection_state(_attr: TokenStream, input: TokenStream) -> TokenStream {
    let input_ast = parse_macro_input!(input as DeriveInput);
    let state_ident = &input_ast.ident;

    let field_names: Vec<_> = match input_ast.data {
        syn::Data::Struct(ref data_struct) => data_struct
            .fields
            .iter()
            .map(|f| f.ident.as_ref().unwrap())
            .collect(),
        _ => panic!("ConnectionState can only be derived on structs"),
    };

    let field_indices: Vec<_> = (0..field_names.len()).collect();

    let expanded = quote! {
        #[derive(Clone, Default, Debug)]
        #input_ast

        struct StateController {
            state: parking_lot::Mutex<#state_ident>,
            channel: std::sync::Arc<tokio::sync::mpsc::Sender<Vec<(usize, Vec<u8>)>>>,
        }

        impl StateController {
            fn new(channel: StateUpdateChannel) -> Self {
                Self {
                    state: parking_lot::Mutex::new(Default::default()),
                    channel: std::sync::Arc::new(channel),
                }
            }

            fn lock(&self) -> StateGuard {
                let state = self.state.lock();
                StateGuard {
                    starting_state: state.clone(),
                    state,
                    channel: self.channel.clone(),
                }
            }
        }

        struct StateGuard<'a> {
            state: parking_lot::MutexGuard<'a, #state_ident>,
            starting_state: #state_ident,
            channel: std::sync::Arc<tokio::sync::mpsc::Sender<Vec<(usize, Vec<u8>)>>>,
        }

        impl<'a> Drop for StateGuard<'a> {
            /// Our custom drop implementation will send any changes to the runtime
            fn drop(&mut self) {
                // "diff" the two states to see what changed
                let mut changes = Vec::new();

                #(
                    if self.state.#field_names != self.starting_state.#field_names {
                        changes.push((
                            #field_indices,
                            rkyv::to_bytes::<_, 1024>(&self.state.#field_names).unwrap().to_vec(),
                        ));
                    }
                )*

                // if there are no changes, don't bother sending anything
                if changes.is_empty() {
                    return;
                }

                // send the changes to the runtime
                // we have to spawn a new task because we can't await inside a drop
                let channel = self.channel.clone();
                tokio::spawn(async move {
                    // this could fail if the server shuts down before these
                    // changes are sent... but we're not too worried about that
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

        impl ClientState for State {
            fn apply_changes(
                &mut self,
                changes: Vec<(usize, Vec<u8>)>,
            ) -> HandlerResult<()> {
                for (field_index, new_value) in changes {
                    match field_index {
                        #(
                            #field_indices => {
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
