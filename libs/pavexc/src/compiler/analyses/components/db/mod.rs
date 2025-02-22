use crate::compiler::analyses::components::component::Component;
use crate::compiler::analyses::components::{
    unregistered::UnregisteredComponent, ConsumptionMode, HydratedComponent, InsertTransformer,
    SourceId,
};
use crate::compiler::analyses::computations::{ComputationDb, ComputationId};
use crate::compiler::analyses::framework_items::FrameworkItemDb;
use crate::compiler::analyses::into_error::register_error_new_transformer;
use crate::compiler::analyses::user_components::{
    ScopeGraph, ScopeId, UserComponent, UserComponentDb, UserComponentId,
};
use crate::compiler::component::{
    Constructor, ConstructorValidationError, ErrorHandler, ErrorObserver, RequestHandler,
    WrappingMiddleware,
};
use crate::compiler::computation::{Computation, MatchResult};
use crate::compiler::interner::Interner;
use crate::compiler::traits::assert_trait_is_implemented;
use crate::compiler::utils::{
    get_err_variant, get_ok_variant, process_framework_callable_path, process_framework_path,
};
use crate::language::{
    Callable, Lifetime, PathType, ResolvedPath, ResolvedPathQualifiedSelf, ResolvedPathSegment,
    ResolvedType, TypeReference,
};
use crate::rustdoc::CrateCollection;
use ahash::{HashMap, HashMapExt, HashSet};
use guppy::graph::PackageGraph;
use indexmap::IndexSet;
use pavex_bp_schema::{CloningStrategy, Lifecycle, Lint, LintSetting};
use std::borrow::Cow;
use std::collections::BTreeMap;

pub(crate) mod diagnostics;

pub(crate) type ComponentId = la_arena::Idx<Component>;

#[derive(Debug)]
pub(crate) struct ComponentDb {
    user_component_db: UserComponentDb,
    interner: Interner<Component>,
    match_err_id2error_handler_id: HashMap<ComponentId, ComponentId>,
    fallible_id2match_ids: HashMap<ComponentId, (ComponentId, ComponentId)>,
    match_id2fallible_id: HashMap<ComponentId, ComponentId>,
    id2transformer_ids: HashMap<ComponentId, IndexSet<ComponentId>>,
    id2lifecycle: HashMap<ComponentId, Lifecycle>,
    /// For each constructor component, determine if it can be cloned or not.
    ///
    /// Invariants: there is an entry for every constructor.
    constructor_id2cloning_strategy: HashMap<ComponentId, CloningStrategy>,
    /// Associate each request handler with the ordered list of middlewares that wrap around it.
    ///
    /// Invariants: there is an entry for every single request handler.
    handler_id2middleware_ids: HashMap<ComponentId, Vec<ComponentId>>,
    /// Associate each request handler with the ordered list of error observer that
    /// must be invoked if something goes wrong.
    ///
    /// Invariants: there is an entry for every single request handler.
    handler_id2error_observer_ids: HashMap<ComponentId, Vec<ComponentId>>,
    /// Associate each transformer with direction on when to apply it.
    ///
    /// Invariants: there is an entry for every single transformer.
    transformer_id2when_to_insert: HashMap<ComponentId, InsertTransformer>,
    /// Associate each error observer with the index of the error input in the list of its
    /// input parameters.
    ///
    /// Invariants: there is an entry for every single error observer.
    error_observer_id2error_input_index: HashMap<ComponentId, usize>,
    error_handler_id2error_handler: HashMap<ComponentId, ErrorHandler>,
    /// A mapping from the low-level [`UserComponentId`]s to the high-level [`ComponentId`]s.
    ///
    /// This is used to "lift" mappings that use [`UserComponentId`] into mappings that
    /// use [`ComponentId`]. In particular:
    ///
    /// - match error handlers with the respective fallible components after they have been
    ///   converted into components.
    /// - match request handlers with the sequence of middlewares that wrap around them.
    /// - convert the ids in the router.
    user_component_id2component_id: HashMap<UserComponentId, ComponentId>,
    /// For each scope, it stores the ordered list of error observers that should be
    /// invoked if a component fails in that scope.
    scope_ids_with_observers: Vec<ScopeId>,
    /// A switch to control if, when a fallible component is registered, [`ComponentDb`]
    /// should automatically register matcher components for its output type.
    autoregister_matchers: bool,
    /// The resolved path to `pavex::Error`.
    /// It's memoised here to avoid re-resolving it multiple times while analysing a single
    /// blueprint.
    pavex_error: PathType,
    /// Users register constructors directly with a blueprint.  
    /// From these user-provided constructors, we build **derived** constructors:
    /// - if a constructor is fallible,
    ///   we create a synthetic constructor to retrieve the Ok variant of its output type
    /// - if a constructor is generic, we create new synthetic constructors by binding its unassigned generic parameters
    ///   to concrete types
    ///
    /// This map holds an entry for each derived constructor.
    /// The value points to the original user-registered constructor it was derived from.
    ///
    /// This dependency relationship can be **indirect**—e.g. an Ok-matcher is derived from a fallible constructor
    /// which was in turn derived
    /// by binding the generic parameters of a user-registered constructor.
    /// The key for the Ok-matcher would point to the user-registered constructor in this scenario,
    /// not to the intermediate derived constructor.
    derived2user_registered: HashMap<ComponentId, ComponentId>,
    /// The id for all framework primitives—i.e. components coming from [`FrameworkItemDb`].
    framework_primitive_ids: HashSet<ComponentId>,
}

/// The `build` method and its auxiliary routines.
impl ComponentDb {
    #[tracing::instrument("Build component database", skip_all)]
    pub fn build(
        user_component_db: UserComponentDb,
        framework_item_db: &FrameworkItemDb,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) -> ComponentDb {
        // We only need to resolve this once.
        let pavex_error = {
            let error = process_framework_path("pavex::Error", package_graph, krate_collection);
            let ResolvedType::ResolvedPath(error) = error else {
                unreachable!()
            };
            error
        };
        let pavex_error_ref = {
            ResolvedType::Reference(TypeReference {
                lifetime: Lifetime::Elided,
                inner: Box::new(ResolvedType::ResolvedPath(pavex_error.clone())),
                is_mutable: false,
            })
        };

        let mut self_ = Self {
            user_component_db,
            interner: Interner::new(),
            match_err_id2error_handler_id: Default::default(),
            fallible_id2match_ids: Default::default(),
            match_id2fallible_id: Default::default(),
            id2transformer_ids: Default::default(),
            id2lifecycle: Default::default(),
            constructor_id2cloning_strategy: Default::default(),
            handler_id2middleware_ids: Default::default(),
            handler_id2error_observer_ids: Default::default(),
            transformer_id2when_to_insert: Default::default(),
            error_observer_id2error_input_index: Default::default(),
            error_handler_id2error_handler: Default::default(),
            user_component_id2component_id: Default::default(),
            scope_ids_with_observers: vec![],
            autoregister_matchers: false,
            pavex_error,
            derived2user_registered: Default::default(),
            framework_primitive_ids: Default::default(),
        };

        {
            // Keep track of which components are fallible to emit a diagnostic
            // if they were not paired with an error handler.
            let mut needs_error_handler = IndexSet::new();

            self_.process_request_handlers(
                &mut needs_error_handler,
                computation_db,
                package_graph,
                krate_collection,
                diagnostics,
            );

            // This **must** be invoked after `process_request_handlers` because it relies on
            // all request handlers being registered to determine which scopes have error observers.
            self_.process_error_observers(
                &pavex_error_ref,
                computation_db,
                package_graph,
                krate_collection,
                diagnostics,
            );

            // We process the backlog of matchers that were not registered during the initial
            // registration phase for request handlers.
            self_.register_all_matchers(computation_db);
            // From this point onwards, all fallible components will automatically get matchers registered.
            // All error matchers will be automatically paired with a conversion into `pavex::error::Error` if needed,
            // based on the scope they belong to.
            self_.autoregister_matchers = true;

            self_.process_constructors(
                &mut needs_error_handler,
                computation_db,
                package_graph,
                krate_collection,
                diagnostics,
            );

            self_.process_wrapping_middlewares(
                &mut needs_error_handler,
                computation_db,
                package_graph,
                krate_collection,
                diagnostics,
            );

            self_.compute_request2middleware_chain();
            self_.process_error_handlers(
                &mut needs_error_handler,
                computation_db,
                package_graph,
                krate_collection,
                diagnostics,
            );

            for fallible_id in needs_error_handler {
                Self::missing_error_handler(
                    fallible_id,
                    &self_.user_component_db,
                    package_graph,
                    diagnostics,
                );
            }
        }

        self_.add_into_response_transformers(
            computation_db,
            package_graph,
            krate_collection,
            diagnostics,
        );

        for (id, type_) in framework_item_db.iter() {
            let component_id = self_.get_or_intern(
                UnregisteredComponent::SyntheticConstructor {
                    computation_id: computation_db.get_or_intern(Constructor(
                        Computation::FrameworkItem(Cow::Owned(type_.clone())),
                    )),
                    scope_id: self_.scope_graph().root_scope_id(),
                    lifecycle: framework_item_db.lifecycle(id),
                    cloning_strategy: framework_item_db.cloning_strategy(id),
                    derived_from: None,
                },
                computation_db,
            );
            self_.framework_primitive_ids.insert(component_id);
        }

        // Add a synthetic constructor for the `pavex::middleware::Next` type.
        {
            let callable = process_framework_callable_path(
                "pavex::middleware::Next::new",
                package_graph,
                krate_collection,
            );
            let computation = Computation::Callable(Cow::Owned(callable));
            self_.get_or_intern(
                UnregisteredComponent::SyntheticConstructor {
                    computation_id: computation_db.get_or_intern(Constructor(computation)),
                    scope_id: self_.scope_graph().root_scope_id(),
                    lifecycle: Lifecycle::RequestScoped,
                    cloning_strategy: CloningStrategy::NeverClone,
                    derived_from: None,
                },
                computation_db,
            );
        }

        self_
    }

    /// Register error and ok matchers for all fallible components.
    fn register_all_matchers(&mut self, computation_db: &mut ComputationDb) {
        let ids: Vec<ComponentId> = self.interner.iter().map(|(id, _)| id).collect();
        for id in ids {
            self.register_matchers(id, computation_db);
        }
    }

    fn get_or_intern(
        &mut self,
        unregistered_component: UnregisteredComponent,
        computation_db: &mut ComputationDb,
    ) -> ComponentId {
        let component = unregistered_component.component();
        let id = self.interner.get_or_intern(component);

        if let Some(user_component_id) = self.user_component_id(id) {
            self.user_component_id2component_id
                .insert(user_component_id, id);
        }

        self.id2lifecycle
            .insert(id, unregistered_component.lifecycle(self));

        {
            use crate::compiler::analyses::components::UnregisteredComponent::*;
            match unregistered_component {
                ErrorHandler {
                    fallible_component_id,
                    error_handler,
                    ..
                } => {
                    self.match_err_id2error_handler_id
                        .insert(self.match_ids(fallible_component_id).unwrap().1, id);
                    self.error_handler_id2error_handler
                        .insert(id, error_handler);
                }
                UserConstructor {
                    user_component_id, ..
                } => {
                    let cloning_strategy = self
                        .user_component_db
                        .get_cloning_strategy(user_component_id)
                        .unwrap();
                    self.constructor_id2cloning_strategy
                        .insert(id, cloning_strategy.to_owned());
                }
                SyntheticConstructor {
                    cloning_strategy,
                    derived_from,
                    ..
                } => {
                    self.constructor_id2cloning_strategy
                        .insert(id, cloning_strategy);
                    if let Some(derived_from) = derived_from {
                        self.derived2user_registered.insert(
                            id,
                            self.derived2user_registered
                                .get(&derived_from)
                                .cloned()
                                .unwrap_or(derived_from),
                        );
                    }
                }
                Transformer {
                    when_to_insert,
                    transformed_component_id,
                    ..
                } => {
                    self.transformer_id2when_to_insert
                        .insert(id, when_to_insert);
                    self.id2transformer_ids
                        .entry(transformed_component_id)
                        .or_default()
                        .insert(id);
                }
                ErrorObserver {
                    error_input_index, ..
                } => {
                    self.error_observer_id2error_input_index
                        .insert(id, error_input_index);
                }
                _ => {}
            }
        }

        if self.autoregister_matchers {
            self.register_matchers(id, computation_db);
        }

        id
    }

    /// Register ok and err matchers for a component if it's fallible.
    fn register_matchers(&mut self, id: ComponentId, computation_db: &mut ComputationDb) {
        let component = self.hydrated_component(id, computation_db);
        let Some(output_type) = component.output_type() else {
            return;
        };
        if !output_type.is_result() || matches!(component, HydratedComponent::Transformer(_)) {
            return;
        }

        let m = MatchResult::match_result(output_type);
        let (ok, err) = (m.ok, m.err);

        // If the component is a constructor, the ok matcher is a constructor.
        // Otherwise it's a transformer.
        let ok_id = match self.hydrated_component(id, computation_db) {
            HydratedComponent::Constructor(_) => {
                let ok_computation_id = computation_db.get_or_intern(ok);
                let ok_id = self.get_or_intern(
                    UnregisteredComponent::SyntheticConstructor {
                        computation_id: ok_computation_id,
                        scope_id: self.scope_id(id),
                        lifecycle: self.lifecycle(id),
                        cloning_strategy: self.constructor_id2cloning_strategy[&id],
                        derived_from: Some(id),
                    },
                    computation_db,
                );
                ok_id
            }
            _ => self.add_synthetic_transformer(
                ok.into(),
                id.into(),
                self.scope_id(id.into()),
                InsertTransformer::Eagerly,
                ConsumptionMode::Move,
                computation_db,
            ),
        };

        let err_id = self.add_synthetic_transformer(
            err.into(),
            id.into(),
            self.scope_id(id.into()),
            InsertTransformer::Eagerly,
            ConsumptionMode::Move,
            computation_db,
        );

        self.fallible_id2match_ids
            .insert(id.into(), (ok_id, err_id));
        self.match_id2fallible_id.insert(ok_id, id.into());
        self.match_id2fallible_id.insert(err_id, id.into());

        // We need to make sure that all error types are upcasted into a `pavex::Error`
        // **if and only if** there is at least one error observer registered.
        let scope_ids = self.scope_ids_with_observers.clone();
        for scope_id in scope_ids {
            register_error_new_transformer(
                err_id,
                self,
                computation_db,
                scope_id,
                &self.pavex_error.clone(),
            );
        }
    }

    /// Validate all user-registered constructors.
    /// We add their information to the relevant metadata stores.
    /// In particular, we keep track of their associated error handler, if one exists.
    fn process_constructors(
        &mut self,
        needs_error_handler: &mut IndexSet<UserComponentId>,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let constructor_ids = self
            .user_component_db
            .constructors()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        for user_component_id in constructor_ids {
            let c: Computation = computation_db[user_component_id].clone().into();
            match TryInto::<Constructor>::try_into(c) {
                Err(e) => {
                    Self::invalid_constructor(
                        e,
                        user_component_id,
                        &self.user_component_db,
                        computation_db,
                        package_graph,
                        krate_collection,
                        diagnostics,
                    );
                }
                Ok(c) => {
                    let constructor_id = self.get_or_intern(
                        UnregisteredComponent::UserConstructor { user_component_id },
                        computation_db,
                    );

                    if c.is_fallible() && self.lifecycle(constructor_id) != Lifecycle::Singleton {
                        // We'll try to match all fallible constructors with an error handler later.
                        // We skip singletons since we don't "handle" errors when constructing them.
                        // They are just bubbled up to the caller by the function that builds
                        // the application state.
                        needs_error_handler.insert(user_component_id);
                    }
                }
            }
        }
    }

    fn process_request_handlers(
        &mut self,
        needs_error_handler: &mut IndexSet<UserComponentId>,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let request_handler_ids = self
            .user_component_db
            .request_handlers()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        for user_component_id in request_handler_ids {
            let callable = &computation_db[user_component_id];
            match RequestHandler::new(Cow::Borrowed(callable)) {
                Err(e) => {
                    Self::invalid_request_handler(
                        e,
                        user_component_id,
                        &self.user_component_db,
                        computation_db,
                        krate_collection,
                        package_graph,
                        diagnostics,
                    );
                }
                Ok(_) => {
                    let id = self.get_or_intern(
                        UnregisteredComponent::RequestHandler { user_component_id },
                        computation_db,
                    );
                    if self.hydrated_component(id, computation_db).is_fallible() {
                        // We'll try to match it with an error handler later.
                        needs_error_handler.insert(user_component_id);
                    }
                }
            }
        }
    }

    fn process_wrapping_middlewares(
        &mut self,
        needs_error_handler: &mut IndexSet<UserComponentId>,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let wrapping_middleware_ids = self
            .user_component_db
            .wrapping_middlewares()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        for user_component_id in wrapping_middleware_ids {
            let user_component = &self.user_component_db[user_component_id];
            let callable = &computation_db[user_component_id];
            let UserComponent::WrappingMiddleware { .. } = user_component else {
                unreachable!()
            };
            match WrappingMiddleware::new(Cow::Borrowed(callable)) {
                Err(e) => {
                    Self::invalid_wrapping_middleware(
                        e,
                        user_component_id,
                        &self.user_component_db,
                        computation_db,
                        krate_collection,
                        package_graph,
                        diagnostics,
                    );
                }
                Ok(_) => {
                    let id = self.get_or_intern(
                        UnregisteredComponent::UserWrappingMiddleware { user_component_id },
                        computation_db,
                    );
                    if self.hydrated_component(id, computation_db).is_fallible() {
                        // We'll try to match it with an error handler later.
                        needs_error_handler.insert(user_component_id);
                    }
                }
            }
        }
    }

    fn process_error_observers(
        &mut self,
        pavex_error_ref: &ResolvedType,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let error_observer_ids = self
            .user_component_db
            .error_observers()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        for user_component_id in error_observer_ids {
            let user_component = &self.user_component_db[user_component_id];
            let callable = &computation_db[user_component_id];
            let UserComponent::ErrorObserver { .. } = user_component else {
                unreachable!()
            };
            match ErrorObserver::new(Cow::Borrowed(callable), pavex_error_ref) {
                Err(e) => {
                    Self::invalid_error_observer(
                        e,
                        user_component_id,
                        &self.user_component_db,
                        computation_db,
                        krate_collection,
                        package_graph,
                        diagnostics,
                    );
                }
                Ok(eo) => {
                    self.get_or_intern(
                        UnregisteredComponent::ErrorObserver {
                            user_component_id,
                            error_input_index: eo.error_input_index,
                        },
                        computation_db,
                    );
                }
            }
        }

        self.compute_request2error_observer_chain();

        let mut v = vec![];
        for component_id in self.request_handler_ids() {
            if self.handler_id2error_observer_ids[&component_id].is_empty() {
                continue;
            }
            v.push(self.scope_id(component_id));
        }
        self.scope_ids_with_observers = v;
    }

    fn process_error_handlers(
        &mut self,
        missing_error_handlers: &mut IndexSet<UserComponentId>,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let iter = self
            .user_component_db
            .iter()
            .filter_map(|(id, c)| {
                use crate::compiler::analyses::user_components::UserComponent::*;
                match c {
                    ErrorHandler {
                        fallible_callable_identifiers_id,
                        ..
                    } => Some((id, *fallible_callable_identifiers_id)),
                    ErrorObserver { .. }
                    | Fallback { .. }
                    | RequestHandler { .. }
                    | Constructor { .. }
                    | WrappingMiddleware { .. } => None,
                }
            })
            .collect::<Vec<_>>();
        for (error_handler_user_component_id, fallible_user_component_id) in iter {
            let lifecycle = self
                .user_component_db
                .get_lifecycle(fallible_user_component_id);
            if lifecycle == Lifecycle::Singleton {
                Self::error_handler_for_a_singleton(
                    error_handler_user_component_id,
                    fallible_user_component_id,
                    &self.user_component_db,
                    package_graph,
                    diagnostics,
                );
                continue;
            }
            let fallible_callable = &computation_db[fallible_user_component_id];
            if fallible_callable.is_fallible() {
                let error_handler_callable = &computation_db[error_handler_user_component_id];
                // Capture immediately that an error handler was registered for this fallible component.
                missing_error_handlers.shift_remove(&fallible_user_component_id);
                match ErrorHandler::new(error_handler_callable.to_owned(), fallible_callable) {
                    Ok(e) => {
                        // This may be `None` if the fallible component failed to pass its own
                        // validation—e.g. the constructor callable was not deemed to be a valid
                        // constructor.
                        if let Some(fallible_component_id) = self
                            .user_component_id2component_id
                            .get(&fallible_user_component_id)
                        {
                            self.get_or_intern(
                                UnregisteredComponent::ErrorHandler {
                                    source_id: error_handler_user_component_id.into(),
                                    fallible_component_id: *fallible_component_id,
                                    error_handler: e,
                                },
                                computation_db,
                            );
                        }
                    }
                    Err(e) => {
                        Self::invalid_error_handler(
                            e,
                            error_handler_user_component_id,
                            &self.user_component_db,
                            computation_db,
                            krate_collection,
                            package_graph,
                            diagnostics,
                        );
                    }
                };
            } else {
                Self::error_handler_for_infallible_component(
                    error_handler_user_component_id,
                    fallible_user_component_id,
                    &self.user_component_db,
                    package_graph,
                    diagnostics,
                );
            }
        }
    }

    /// Compute the middleware chain for each request handler that was successfully validated.
    /// The middleware chain only includes wrapping middlewares that were successfully validated.
    /// Invalid middlewares are ignored.
    fn compute_request2middleware_chain(&mut self) {
        for (request_handler_id, _) in self.user_component_db.request_handlers() {
            let Some(handler_component_id) =
                self.user_component_id2component_id.get(&request_handler_id)
            else {
                continue;
            };
            let mut middleware_chain = vec![];
            for middleware_id in self
                .user_component_db
                .get_middleware_ids(request_handler_id)
            {
                if let Some(middleware_component_id) =
                    self.user_component_id2component_id.get(middleware_id)
                {
                    middleware_chain.push(*middleware_component_id);
                }
            }
            self.handler_id2middleware_ids
                .insert(*handler_component_id, middleware_chain);
        }
    }

    /// Compute the list of error observers for each request handler that was successfully validated.
    /// The list only includes error observers that were successfully validated.
    /// Invalid error observers are ignored.
    fn compute_request2error_observer_chain(&mut self) {
        for (request_handler_id, _) in self.user_component_db.request_handlers() {
            let Some(handler_component_id) =
                self.user_component_id2component_id.get(&request_handler_id)
            else {
                continue;
            };
            let mut chain = vec![];
            for id in self
                .user_component_db
                .get_error_observer_ids(request_handler_id)
            {
                if let Some(component_id) = self.user_component_id2component_id.get(id) {
                    chain.push(*component_id);
                }
            }
            self.handler_id2error_observer_ids
                .insert(*handler_component_id, chain);
        }
    }

    /// We need to make sure that all output nodes return the same output type.
    /// We do this by adding a "response transformer" node that converts the output type to a
    /// common type—`pavex::response::Response`.
    fn add_into_response_transformers(
        &mut self,
        computation_db: &mut ComputationDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let into_response = {
            let into_response = process_framework_path(
                "pavex::response::IntoResponse",
                package_graph,
                krate_collection,
            );
            let ResolvedType::ResolvedPath(into_response) = into_response else {
                unreachable!()
            };
            into_response
        };
        let into_response_path = into_response.resolved_path();
        let iter: Vec<_> = self
            .interner
            .iter()
            .filter_map(|(id, c)| {
                use crate::compiler::analyses::components::component::Component::*;

                match c {
                    RequestHandler { user_component_id }
                    | WrappingMiddleware {
                        // There are no error handlers with a `ComputationId` source at this stage.
                        source_id: SourceId::UserComponentId(user_component_id),
                    }
                    | ErrorHandler {
                        // There are no error handlers with a `ComputationId` source at this stage.
                        source_id: SourceId::UserComponentId(user_component_id),
                    } => Some((id, *user_component_id)),
                    Constructor { .. }
                    | Transformer { .. }
                    | WrappingMiddleware { .. }
                    | ErrorObserver { .. }
                    | ErrorHandler { .. } => None,
                }
            })
            .collect();
        for (component_id, user_component_id) in iter.into_iter() {
            // If the component is fallible, we want to attach the transformer to its Ok matcher.
            let component_id =
                if let Some((ok_id, _)) = self.fallible_id2match_ids.get(&component_id) {
                    *ok_id
                } else {
                    component_id
                };
            let callable = &computation_db[user_component_id];
            let output = callable.output.as_ref().unwrap();
            let output = if output.is_result() {
                get_ok_variant(output)
            } else {
                output
            }
            .to_owned();
            if let Err(e) = assert_trait_is_implemented(krate_collection, &output, &into_response) {
                Self::invalid_response_type(
                    e,
                    &output,
                    user_component_id,
                    &self.user_component_db,
                    package_graph,
                    diagnostics,
                );
                continue;
            }
            let mut transformer_segments = into_response_path.segments.clone();
            transformer_segments.push(ResolvedPathSegment {
                ident: "into_response".into(),
                generic_arguments: vec![],
            });
            let transformer_path = ResolvedPath {
                segments: transformer_segments,
                qualified_self: Some(ResolvedPathQualifiedSelf {
                    position: into_response_path.segments.len(),
                    type_: output.clone().into(),
                }),
                package_id: into_response_path.package_id.clone(),
            };
            match computation_db.resolve_and_intern(krate_collection, &transformer_path, None) {
                Ok(callable_id) => {
                    let transformer = UnregisteredComponent::Transformer {
                        computation_id: callable_id,
                        transformed_component_id: component_id,
                        transformation_mode: ConsumptionMode::Move,
                        scope_id: self.scope_id(component_id),
                        when_to_insert: InsertTransformer::Eagerly,
                    };
                    self.get_or_intern(transformer, computation_db);
                }
                Err(e) => {
                    Self::cannot_handle_into_response_implementation(
                        e,
                        &output,
                        user_component_id,
                        &self.user_component_db,
                        package_graph,
                        diagnostics,
                    );
                }
            }
        }
    }
}

impl ComponentDb {
    fn add_synthetic_transformer(
        &mut self,
        computation: Computation<'static>,
        transformed_id: ComponentId,
        scope_id: ScopeId,
        when_to_insert: InsertTransformer,
        consumption_mode: ConsumptionMode,
        computation_db: &mut ComputationDb,
    ) -> ComponentId {
        let computation_id = computation_db.get_or_intern(computation);
        self.get_or_intern_transformer(
            computation_id,
            transformed_id,
            scope_id,
            when_to_insert,
            consumption_mode,
            computation_db,
        )
    }

    pub fn get_or_intern_constructor(
        &mut self,
        callable_id: ComputationId,
        lifecycle: Lifecycle,
        scope_id: ScopeId,
        cloning_strategy: CloningStrategy,
        computation_db: &mut ComputationDb,
        derived_from: Option<ComponentId>,
    ) -> Result<ComponentId, ConstructorValidationError> {
        let callable = computation_db[callable_id].to_owned();
        TryInto::<Constructor>::try_into(callable)?;
        let constructor_component = UnregisteredComponent::SyntheticConstructor {
            lifecycle,
            computation_id: callable_id,
            scope_id,
            cloning_strategy,
            derived_from,
        };
        Ok(self.get_or_intern(constructor_component, computation_db))
    }

    pub fn get_or_intern_wrapping_middleware(
        &mut self,
        callable: Cow<'_, Callable>,
        scope_id: ScopeId,
        computation_db: &mut ComputationDb,
    ) -> ComponentId {
        let computation = Computation::Callable(callable).into_owned();
        let computation_id = computation_db.get_or_intern(computation);
        let middleware_component = UnregisteredComponent::SyntheticWrappingMiddleware {
            computation_id,
            scope_id,
        };
        let middleware_id = self.get_or_intern(middleware_component, computation_db);
        middleware_id
    }

    pub fn get_or_intern_transformer(
        &mut self,
        callable_id: ComputationId,
        transformed_component_id: ComponentId,
        scope_id: ScopeId,
        when_to_insert: InsertTransformer,
        consumption_mode: ConsumptionMode,
        computation_db: &mut ComputationDb,
    ) -> ComponentId {
        let transformer = UnregisteredComponent::Transformer {
            computation_id: callable_id,
            transformed_component_id,
            transformation_mode: consumption_mode,
            scope_id,
            when_to_insert,
        };
        self.get_or_intern(transformer, computation_db)
    }

    /// Retrieve the lifecycle for a component.
    pub fn lifecycle(&self, id: ComponentId) -> Lifecycle {
        self.id2lifecycle[&id]
    }

    /// Retrieve the lint overrides for a component.
    pub fn lints(&self, id: ComponentId) -> Option<&BTreeMap<Lint, LintSetting>> {
        let Some(user_component_id) = self.user_component_id(id) else {
            return None;
        };
        self.user_component_db.get_lints(user_component_id)
    }

    /// The mapping from a low-level [`UserComponentId`] to its corresponding [`ComponentId`].
    pub fn user_component_id2component_id(&self) -> &HashMap<UserComponentId, ComponentId> {
        &self.user_component_id2component_id
    }

    /// Iterate over all the components in the database alongside their ids.
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (ComponentId, &Component)> + ExactSizeIterator + DoubleEndedIterator
    {
        self.interner.iter()
    }

    /// If the component is an error match node, return the id of the
    /// error handler designated to handle the error.
    /// Otherwise, return `None`.
    pub fn error_handler_id(&self, err_match_id: ComponentId) -> Option<&ComponentId> {
        self.match_err_id2error_handler_id.get(&err_match_id)
    }

    /// If the component is a transformer, return the expected insertion behaviour.
    /// Panic otherwise.
    pub fn when_to_insert(&self, transformer_id: ComponentId) -> InsertTransformer {
        self.transformer_id2when_to_insert[&transformer_id]
    }

    /// If the component is a request handler, return the ids of the middlewares that wrap around
    /// it.
    /// Otherwise, return `None`.
    pub fn middleware_chain(&self, handler_id: ComponentId) -> Option<&[ComponentId]> {
        self.handler_id2middleware_ids
            .get(&handler_id)
            .map(|v| &v[..])
    }

    /// If the component is a request handler, return the ids of the error observers that must be
    /// invoked when something goes wrong in the request processing pipeline.  
    /// Otherwise, return `None`.
    pub fn error_observers(&self, handler_id: ComponentId) -> Option<&[ComponentId]> {
        self.handler_id2error_observer_ids
            .get(&handler_id)
            .map(|v| &v[..])
    }

    /// If transformations must be applied to the component, return their ids.
    /// Otherwise, return `None`.
    pub fn transformer_ids(&self, component_id: ComponentId) -> Option<&IndexSet<ComponentId>> {
        self.id2transformer_ids.get(&component_id)
    }

    /// If the component is fallible, return the id of the `MatchResult` components for the `Ok`
    /// and the `Err` variants.
    /// If the component is infallible, return `None`.
    pub fn match_ids(
        &self,
        fallible_component_id: ComponentId,
    ) -> Option<&(ComponentId, ComponentId)> {
        self.fallible_id2match_ids.get(&fallible_component_id)
    }

    /// Return the ids of the components that are derived from the given constructor.
    /// E.g. if the constructor is a fallible constructor, the derived components are the
    /// `MatchResult` components for the `Ok` and `Err` variants (and their respective
    /// derived components).
    /// If the constructor is a non-fallible constructor, the derived components are the
    /// `BorrowSharedReference` component.
    pub fn derived_component_ids(&self, component_id: ComponentId) -> Vec<ComponentId> {
        let mut derived_ids = Vec::new();
        if let Some(match_ids) = self.match_ids(component_id) {
            derived_ids.push(match_ids.0);
            derived_ids.push(match_ids.1);
            derived_ids.extend(self.derived_component_ids(match_ids.0));
            derived_ids.extend(self.derived_component_ids(match_ids.1));
        }
        derived_ids
    }

    /// Return the id of user-registered component that `component_id` was derived from
    /// (e.g. an Ok-matcher is derived from a fallible constructor or
    /// a bound constructor from a generic user-registered one).
    ///
    /// **It only works for constructors**.
    pub fn derived_from(&self, component_id: &ComponentId) -> Option<ComponentId> {
        self.derived2user_registered.get(component_id).cloned()
    }

    /// Returns `true` if the component is a framework primitive (i.e. it comes from
    /// [`FrameworkItemDb`].
    /// `false` otherwise.
    pub fn is_framework_primitive(&self, component_id: &ComponentId) -> bool {
        self.framework_primitive_ids.contains(component_id)
    }

    /// Given the id of a [`MatchResult`] component, return the id of the corresponding fallible
    /// component.
    #[track_caller]
    pub fn fallible_id(&self, match_component_id: ComponentId) -> ComponentId {
        self.match_id2fallible_id[&match_component_id]
    }

    #[track_caller]
    /// Given the id of a component, return the corresponding [`CloningStrategy`].
    /// It panics if called for a non-constructor component.
    pub fn cloning_strategy(&self, component_id: ComponentId) -> CloningStrategy {
        self.constructor_id2cloning_strategy[&component_id]
    }

    /// Iterate over all constructors in the component database, either user-provided or synthetic.
    pub fn constructors<'a>(
        &'a self,
        computation_db: &'a ComputationDb,
    ) -> impl Iterator<Item = (ComponentId, Constructor<'a>)> {
        self.interner.iter().filter_map(|(id, c)| {
            let Component::Constructor { source_id } = c else {
                return None;
            };
            let computation = match source_id {
                SourceId::ComputationId(id, _) => computation_db[*id].clone(),
                SourceId::UserComponentId(id) => computation_db[*id].clone().into(),
            };
            Some((id, Constructor(computation)))
        })
    }

    /// Iterate over all the request handlers in the component database.
    pub fn request_handler_ids(&self) -> impl Iterator<Item = ComponentId> + '_ {
        self.interner.iter().filter_map(|(id, c)| {
            let Component::RequestHandler { .. } = c else {
                return None;
            };
            Some(id)
        })
    }

    pub(crate) fn user_component_id(&self, id: ComponentId) -> Option<UserComponentId> {
        match &self[id] {
            Component::Constructor {
                source_id: SourceId::UserComponentId(user_component_id),
            }
            | Component::ErrorHandler {
                source_id: SourceId::UserComponentId(user_component_id),
            }
            | Component::WrappingMiddleware {
                source_id: SourceId::UserComponentId(user_component_id),
            }
            | Component::ErrorObserver { user_component_id }
            | Component::RequestHandler { user_component_id } => Some(*user_component_id),
            Component::ErrorHandler {
                source_id: SourceId::ComputationId(..),
            }
            | Component::Constructor {
                source_id: SourceId::ComputationId(..),
            }
            | Component::WrappingMiddleware {
                source_id: SourceId::ComputationId(..),
            }
            | Component::Transformer { .. } => None,
        }
    }

    pub(crate) fn hydrated_component<'a, 'b: 'a>(
        &'a self,
        id: ComponentId,
        computation_db: &'b ComputationDb,
    ) -> HydratedComponent<'a> {
        let component = &self[id];
        match component {
            Component::RequestHandler { user_component_id } => {
                let callable = &computation_db[*user_component_id];
                let request_handler = RequestHandler {
                    callable: Cow::Borrowed(callable),
                };
                HydratedComponent::RequestHandler(request_handler)
            }
            Component::WrappingMiddleware { source_id } => {
                let c = match source_id {
                    SourceId::ComputationId(id, _) => computation_db[*id].clone(),
                    SourceId::UserComponentId(id) => computation_db[*id].clone().into(),
                };
                let Computation::Callable(callable) = c else {
                    unreachable!()
                };
                let w = WrappingMiddleware { callable };
                HydratedComponent::WrappingMiddleware(w)
            }
            Component::ErrorHandler { .. } => {
                let error_handler = &self.error_handler_id2error_handler[&id];
                HydratedComponent::ErrorHandler(Cow::Borrowed(error_handler))
            }
            Component::Constructor { source_id } => {
                let c = match source_id {
                    SourceId::ComputationId(id, _) => computation_db[*id].clone(),
                    SourceId::UserComponentId(id) => computation_db[*id].clone().into(),
                };
                HydratedComponent::Constructor(Constructor(c))
            }
            Component::Transformer { computation_id, .. } => {
                let c = &computation_db[*computation_id];
                HydratedComponent::Transformer(c.clone())
            }
            Component::ErrorObserver { user_component_id } => {
                let callable = &computation_db[*user_component_id];
                let error_observer = ErrorObserver {
                    callable: Cow::Borrowed(callable),
                    error_input_index: self.error_observer_id2error_input_index[&id],
                };
                HydratedComponent::ErrorObserver(error_observer)
            }
        }
    }

    /// Return the [`UserComponentDb`] used as a seed for this component database.
    pub fn user_component_db(&self) -> &UserComponentDb {
        &self.user_component_db
    }

    /// Return the [`ScopeGraph`] that backs the [`ScopeId`]s for this component database.
    pub fn scope_graph(&self) -> &ScopeGraph {
        self.user_component_db.scope_graph()
    }

    /// Return the [`ScopeId`] of the given component.
    pub fn scope_id(&self, component_id: ComponentId) -> ScopeId {
        match &self[component_id] {
            Component::RequestHandler { user_component_id }
            | Component::ErrorObserver { user_component_id } => {
                self.user_component_db[*user_component_id].scope_id()
            }
            Component::WrappingMiddleware { source_id }
            | Component::Constructor { source_id }
            | Component::ErrorHandler { source_id } => match source_id {
                SourceId::ComputationId(_, scope_id) => *scope_id,
                SourceId::UserComponentId(id) => self.user_component_db[*id].scope_id(),
            },
            Component::Transformer { scope_id, .. } => *scope_id,
        }
    }
}

impl ComponentDb {
    /// Print to stdout a debug dump of the component database, primarily for debugging
    /// purposes.
    #[allow(unused)]
    pub(crate) fn debug_dump(&self, computation_db: &ComputationDb) {
        for (component_id, _) in self.iter() {
            println!(
                "Component id: {:?}\nHydrated component: {:?}\nLifecycle: {:?}",
                component_id,
                self.hydrated_component(component_id, computation_db),
                self.lifecycle(component_id)
            );

            println!("Matchers:");
            if let Some((ok_id, err_id)) = self.match_ids(component_id) {
                let matchers = format!(
                    "- Ok: {:?}\n- Err: {:?}",
                    self.hydrated_component(*ok_id, computation_db),
                    self.hydrated_component(*err_id, computation_db)
                );
                println!("{}", textwrap::indent(&matchers, "  "));
            }
            println!("Error handler:");
            if let Some(err_handler_id) = self.error_handler_id(component_id) {
                let error_handler = format!(
                    "{:?}",
                    self.hydrated_component(*err_handler_id, computation_db)
                );
                println!("{}", textwrap::indent(&error_handler, "  "));
            }
            println!("Transformers:");
            if let Some(transformer_ids) = self.transformer_ids(component_id) {
                let transformers = transformer_ids
                    .iter()
                    .map(|id| format!("- {:?}", self.hydrated_component(*id, computation_db)))
                    .collect::<Vec<_>>()
                    .join("\n");
                println!("{}", textwrap::indent(&transformers, "  "));
            }
            println!();
        }
    }
}

// All methods related to the logic for binding generic components.
impl ComponentDb {
    pub fn bind_generic_type_parameters(
        &mut self,
        id: ComponentId,
        bindings: &HashMap<String, ResolvedType>,
        computation_db: &mut ComputationDb,
    ) -> ComponentId {
        fn _get_root_component_id(
            component_id: ComponentId,
            component_db: &ComponentDb,
            computation_db: &ComputationDb,
        ) -> ComponentId {
            let templated_component = component_db
                .hydrated_component(component_id, computation_db)
                .into_owned();
            match templated_component {
                HydratedComponent::WrappingMiddleware(_) => component_id,
                // We want to make sure we are binding the root component (i.e. a constructor registered
                // by the user), not a derived one. If not, we might have resolution issues when computing
                // the call graph for handlers where these derived components are used.
                HydratedComponent::Constructor(constructor) => match &constructor.0 {
                    Computation::FrameworkItem(_) | Computation::Callable(_) => component_id,
                    Computation::MatchResult(_) => _get_root_component_id(
                        component_db.fallible_id(component_id),
                        component_db,
                        computation_db,
                    ),
                },
                HydratedComponent::RequestHandler(_)
                | HydratedComponent::ErrorHandler(_)
                | HydratedComponent::ErrorObserver(_)
                | HydratedComponent::Transformer(_) => {
                    todo!()
                }
            }
        }

        let id = _get_root_component_id(id, self, computation_db);
        let scope_id = self.scope_id(id);

        let bound_component_id = match self.hydrated_component(id, computation_db).into_owned() {
            HydratedComponent::Constructor(constructor) => {
                let cloning_strategy = self.constructor_id2cloning_strategy[&id];
                let bound_computation = constructor
                    .0
                    .bind_generic_type_parameters(bindings)
                    .into_owned();
                let bound_computation_id = computation_db.get_or_intern(bound_computation);

                // This registers all "derived" constructors as well (borrowed references, matchers, etc.)
                // but it doesn't take care of the error handler, in case `id` pointed to a fallible constructor.
                // We need to do that manually.
                self.get_or_intern_constructor(
                    bound_computation_id,
                    self.lifecycle(id),
                    scope_id,
                    cloning_strategy,
                    computation_db,
                    Some(id),
                )
                .unwrap()
            }
            HydratedComponent::WrappingMiddleware(mw) => {
                let bound_callable = mw.callable.bind_generic_type_parameters(bindings);
                self.get_or_intern_wrapping_middleware(
                    Cow::Owned(bound_callable),
                    scope_id,
                    computation_db,
                )
            }
            HydratedComponent::RequestHandler(_)
            | HydratedComponent::ErrorHandler(_)
            | HydratedComponent::ErrorObserver(_)
            | HydratedComponent::Transformer(_) => {
                todo!()
            }
        };

        if let Some((_, err_match_id)) = self.fallible_id2match_ids.get(&id) {
            let err_handler_id = self.match_err_id2error_handler_id[err_match_id];
            let HydratedComponent::ErrorHandler(error_handler) =
                self.hydrated_component(err_handler_id, computation_db)
            else {
                unreachable!()
            };

            // `bindings` contains the concrete types for all the unassigned generic
            // type parameters that appear in the signature of the templated component.
            // The error handler might itself have unassigned generic parameters that are
            // _equivalent_ to those in the fallible component, but named differently.
            //
            // E.g.
            // - Constructor: `fn constructor<T>(x: u64) -> Result<T, Error<T>>`
            // - Error handler: `fn error_handler<S>(e: &Error<S>) -> Response`
            //
            // This little utility function "adapts" the bindings from the naming of the fallible
            // component to the ones required by the error handler.
            let error_handler_bindings = {
                let templated_output = self
                    .hydrated_component(id, computation_db)
                    .output_type()
                    .unwrap()
                    .to_owned();
                let ref_component_error_type = ResolvedType::Reference(TypeReference {
                    is_mutable: false,
                    lifetime: Lifetime::Elided,
                    inner: Box::new(get_err_variant(&templated_output).to_owned()),
                });
                let ref_error_handler_error_type = error_handler.error_type_ref();

                let remapping = ref_component_error_type
                    .is_equivalent_to(ref_error_handler_error_type)
                    .unwrap();
                let mut error_handler_bindings = HashMap::new();
                for (generic, concrete) in bindings {
                    // `bindings` contains the concrete types for all the unassigned generic
                    // type parameters that appear in the signature of the templated component.
                    // It is not guaranteed that ALL those generic type parameters appear in the
                    // signature of the error handler, so we need to mindful here.
                    //
                    // E.g.
                    // - Constructor: `fn constructor<T>(x: u64) -> Result<T, Error>`
                    // - Error handler: `fn error_handler(e: &Error) -> Response`
                    if let Some(error_handler_generic) = remapping.get(generic.as_str()) {
                        error_handler_bindings
                            .insert((*error_handler_generic).to_owned(), concrete.clone());
                    }
                }
                error_handler_bindings
            };

            let bound_error_handler =
                error_handler.bind_generic_type_parameters(&error_handler_bindings);
            let bound_computation =
                Computation::Callable(Cow::Borrowed(&bound_error_handler.callable)).into_owned();
            let bound_error_handler_computation_id =
                computation_db.get_or_intern(bound_computation);
            let bound_error_component_id = self.get_or_intern(
                UnregisteredComponent::ErrorHandler {
                    source_id: SourceId::ComputationId(
                        bound_error_handler_computation_id,
                        scope_id,
                    ),
                    fallible_component_id: bound_component_id,
                    error_handler: bound_error_handler,
                },
                computation_db,
            );

            // Finally, we need to bound the error handler's transformers.
            if let Some(transformer_ids) = self.transformer_ids(err_handler_id).cloned() {
                for transformer_id in transformer_ids {
                    let consumption_mode = match &self[transformer_id] {
                        Component::Transformer {
                            transformation_mode,
                            ..
                        } => *transformation_mode,
                        _ => unreachable!(),
                    };
                    let HydratedComponent::Transformer(transformer) =
                        self.hydrated_component(transformer_id, computation_db)
                    else {
                        unreachable!()
                    };
                    let insert_mode = self.transformer_id2when_to_insert[&transformer_id];
                    let bound_transformer = transformer
                        .bind_generic_type_parameters(bindings)
                        .into_owned();
                    self.add_synthetic_transformer(
                        bound_transformer,
                        bound_error_component_id,
                        scope_id,
                        insert_mode,
                        consumption_mode,
                        computation_db,
                    );
                }
            }
        }

        bound_component_id
    }
}

impl std::ops::Index<ComponentId> for ComponentDb {
    type Output = Component;

    fn index(&self, index: ComponentId) -> &Self::Output {
        &self.interner[index]
    }
}
