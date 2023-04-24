use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, AttributeArgs, ItemTrait, Ident, ItemStruct};

#[proc_macro_attribute]
pub fn connection_state(_attr: TokenStream, input: TokenStream) -> TokenStream {
    let input_ast = parse_macro_input!(input as ItemStruct);
    let state_ident = &input_ast.ident;
    let vis = &input_ast.vis;

    let field_names: Vec<&Ident> = input_ast
        .fields
        .iter()
        .map(|field| field.ident.as_ref().unwrap())
        .collect();

    let field_indices: Vec<usize> = (0..field_names.len()).collect();

    let expanded = quote! {
        #[derive(Clone, Default, Debug)]
        #input_ast

        #vis struct StateController {
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

        #vis struct StateGuard<'a> {
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

fn snake_to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

#[proc_macro_attribute]
pub fn rpc(args: TokenStream, input: TokenStream) -> TokenStream {
    let _args = parse_macro_input!(args as AttributeArgs);
    let trait_input = parse_macro_input!(input as ItemTrait);
    let vis = &trait_input.vis;

    let trait_ident = &trait_input.ident;

    // Generate RpcCall enum variants
    let rpc_variants = trait_input
        .items
        .iter()
        .filter_map(|item| {
            if let syn::TraitItem::Method(method) = item {
                let variant_ident = {
                    let s = method.sig.ident.to_string();
                    format_ident!("{}", snake_to_pascal_case(&s))
                };

                // inputs need to be like ident: type so we can use them in the
                // enum
                let inputs = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|input| {
                        if let syn::FnArg::Typed(typed) = input {
                            if let syn::Pat::Ident(ident) = &*typed.pat {
                                let ty = &typed.ty;
                                Some(quote! {
                                    #ident: #ty
                                })
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                Some(quote! {
                    #variant_ident { #(#inputs),* }
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let shared_code = quote! {
        #[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
        #[archive_attr(derive(bytecheck::CheckBytes))]
        #[repr(u8)]
        #vis enum RpcCall {
            #(#rpc_variants),*
        }
    };

    let application_server = {
        let server_struct_ident = format_ident!("{}Server", trait_ident);

        quote! {
            #vis struct #server_struct_ident {
                config: ServerConfig,
                shutdown: Option<tokio::sync::oneshot::Sender<()>>,
                control_channels: Option<()>,
            }

            #[async_trait::async_trait]
            impl ApplicationServer for #server_struct_ident {
                fn new(config: ServerConfig) -> Self {
                    Self {
                        config,
                        shutdown: None,
                        control_channels: None,
                    }
                }

                async fn start(&mut self) -> Result<(), std::io::Error> {
                    let server = Server::new(self.config.clone(), Handler::init());
                    let (error_tx, error_rx) = tokio::sync::oneshot::channel();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (control_channels_tx, control_channels_rx) = tokio::sync::oneshot::channel();

                    tokio::spawn(async move {
                        if let Err(e) = server.run(shutdown_rx, control_channels_tx).await {
                            error_tx.send(e).unwrap()
                        };
                    });

                    tokio::select! {
                        e = error_rx => {
                            tracing::error!("Server error: {:?}", e);
                            return Err(e.unwrap());
                        }
                        control_channels = control_channels_rx => {
                            self.control_channels = Some(control_channels.unwrap());
                            self.shutdown = Some(shutdown_tx);
                            Ok(())
                        }
                    }
                }
                fn stop(&mut self) {
                    tracing::debug!("Telling server to shutdown");
                    match self.shutdown.take() {
                        Some(shutdown) => {
                            let _ = shutdown.send(());
                        }
                        None => {}
                    }
                }
            }
        }
    };

    let server_handler = {
        let server_methods = trait_input
            .items
            .iter()
            .filter_map(|item| {
                if let syn::TraitItem::Method(method) = item {
                    let method_ident = &method.sig.ident;
                    let variant_ident = {
                        let s = method.sig.ident.to_string();
                        format_ident!("{}", snake_to_pascal_case(&s))
                    };
                    let inputs = method
                        .sig
                        .inputs
                        .iter()
                        .filter_map(|arg| match arg {
                            syn::FnArg::Typed(pat) => Some(&pat.pat),
                            _ => None,
                        })
                        .collect::<Vec<_>>();

                    Some(quote! {
                        RpcCall::#variant_ident { #(#inputs),* } => {
                            let result = self.#method_ident(#(#inputs),*).await?;
                            let result = rkyv::to_bytes::<_, 1024>(&result).unwrap();
                            Ok(result.to_vec())
                        }
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        quote! {
            /// RPC server that implements the [Counter] trait. A wrapper around
            /// [Server]
            #vis struct Handler {
                // the runtime will provide the state when it creates the handler
                state: std::sync::Arc<StateController>,
            }

            impl Handler {
                /// An easier way to get the channel factory
                fn init(
                ) -> impl Fn(StateUpdateChannel) -> Box<dyn ServerHandler + Send + Sync>
                       + Send
                       + Sync
                       + 'static
                       + Copy {
                    |state_update_channel| Box::new(Self::new(state_update_channel))
                }
            }

            #[async_trait::async_trait]
            impl ServerHandler for Handler {
                fn new(state_update_channel: StateUpdateChannel) -> Self {
                    Self {
                        state: std::sync::Arc::new(StateController::new(state_update_channel)),
                    }
                }

                async fn handle_rpc_call(
                    &self,
                    input: &[u8],
                ) -> Result<Vec<u8>, RpcHandlerError> {
                    let call: RpcCall = rkyv::from_bytes(input)
                        .map_err(|_| RpcHandlerError::BadInputBytes)?;

                    match call {
                        #(#server_methods),*
                    }
                }
            }
        }
    };

    let application_client = {
        // Generate the client code
        let client_name = format_ident!("{}Client", trait_ident);

        let mut client_methods = Vec::new();
        for item in &trait_input.items {
            if let syn::TraitItem::Method(method) = item {
                let method_ident = &method.sig.ident;
                let method_inputs = &method.sig.inputs;
                let method_output = &method.sig.output;

                let rpc_call_variant = {
                    let s = method_ident.to_string();
                    format_ident!("{}", snake_to_pascal_case(&s))
                };

                let rpc_call_params = method_inputs
                    .iter()
                    .filter_map(|arg| match arg {
                        syn::FnArg::Typed(pat_type) => Some(&pat_type.pat),
                        _ => None,
                    })
                    .collect::<Vec<_>>();

                let client_method = quote! {
                    async fn #method_ident(#method_inputs) #method_output {
                        match self.make_rpc_call(RpcCall::#rpc_call_variant { #(#rpc_call_params),* }).await {
                            Ok(c) => rkyv::from_bytes(&c)
                                .map_err(|_| RpcHandlerError::BadOutputBytes),
                            Err(e) => Err(e),
                        }
                    }
                };

                client_methods.push(client_method);
            }
        }

        quote! {
            // CLIENT CODE
            #vis struct #client_name {
                host: String,
                self_signed: bool,
                shutdown: Option<tokio::sync::oneshot::Sender<()>>,
                rpc_tx: Option<tokio::sync::mpsc::Sender<(Vec<u8>, RpcResponseSender)>>,
            }

            #[async_trait::async_trait]
            impl ApplicationClient for #client_name {
                fn new_self_signed(host: &str) -> Self {
                    Self {
                        host: host.to_string(),
                        self_signed: true,
                        shutdown: None,
                        rpc_tx: None,
                    }
                }

                #[allow(dead_code)]
                fn new(host: &str) -> Self {
                    Self {
                        host: host.to_string(),
                        self_signed: false,
                        shutdown: None,
                        rpc_tx: None,
                    }
                }

                /// Spawns a runtime client in the background to maintain the active
                /// connection
                async fn connect(&mut self) -> Result<(), tungstenite::Error> {
                    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (control_channels_tx, control_channels_rx) = tokio::sync::oneshot::channel();
                    let (error_tx, error_rx) = tokio::sync::oneshot::channel();

                    let self_signed = self.self_signed;
                    let host = self.host.clone();

                    tokio::spawn(async move {
                        let mut client: Client<State> = if self_signed {
                            Client::new_self_signed(&host)
                        } else {
                            Client::new(&host)
                        };

                        if let Err(e) =
                            client.connect(shutdown_rx, control_channels_tx).await
                        {
                            error_tx.send(e).unwrap()
                        };
                    });

                    tokio::select! {
                        Ok((rpc_tx,)) = control_channels_rx => {
                            // at this point, the client will NOT return any errors, so we
                            // can safely ignore the error_rx channel
                            tracing::debug!("Received control channels from client");
                            self.shutdown = Some(shutdown);
                            self.rpc_tx = Some(rpc_tx);
                            Ok(())
                        }
                        e = error_rx => {
                            tracing::error!("Error received from client: {:?}", e);
                            Err(e.unwrap())
                        }
                    }
                }

                fn disconnect(&mut self) {
                    tracing::debug!("Telling client to shutdown");
                    match self.shutdown.take() {
                        Some(shutdown) => {
                            let _ = shutdown.send(());
                        }
                        None => {}
                    }
                }
            }

            impl #client_name {
                async fn make_rpc_call(&self, call: RpcCall) -> HandlerResult<Vec<u8>> {
                    if let Some(rpc_chan) = self.rpc_tx.clone() {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        rpc_chan
                            .send((
                                rkyv::to_bytes::<RpcCall, 1024>(&call)
                                    .map_err(|_| RpcHandlerError::BadInputBytes)?
                                    .to_vec(),
                                tx,
                            ))
                            .await
                            .unwrap();
                        rx.await.unwrap()
                    } else {
                        Err(RpcHandlerError::ClientNotConnected)
                    }
                }
            }

            impl Drop for #client_name {
                fn drop(&mut self) {
                    tracing::debug!("Application client got dropped. Disconnecting.");
                    self.disconnect();
                }
            }

            #[async_trait::async_trait]
            impl #trait_ident for #client_name {
                #(#client_methods)*
            }
        }
    };

    let expanded = quote! {
        #[async_trait::async_trait]
        #trait_input
        #shared_code
        #application_server
        #server_handler
        #application_client
    };

    TokenStream::from(expanded)
}

#[proc_macro_attribute]
/// This attribute is used to mark a struct as an RPC handler. Currently,
/// this just adds the `#[async_trait::async_trait]` attribute to the struct.
pub fn rpc_handler(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as syn::ItemImpl);

    let expanded = quote! {
        #[async_trait::async_trait]
        #input
    };

    proc_macro::TokenStream::from(expanded)
}

#[proc_macro_attribute]
// takes any ast as an input and annotates it with rkyv's macros
pub fn codable(_attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as syn::Item);

    let expanded = quote! {
        #[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, PartialEq)]
        #[archive_attr(derive(PartialEq, rkyv::CheckBytes))]
        #input
    };

    proc_macro::TokenStream::from(expanded)
}