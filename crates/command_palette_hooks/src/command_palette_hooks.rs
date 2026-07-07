//! Provides hooks for customizing the behavior of the command palette.

#![deny(missing_docs)]

use std::{any::TypeId, rc::Rc};

use collections::{HashSet, TypeIdHashSet};
use derive_more::{Deref, DerefMut};
use gpui::{Action, App, BorrowAppContext, Global, Task, WeakEntity};
use workspace::Workspace;

/// Initializes the command palette hooks.
pub fn init(cx: &mut App) {
    cx.set_global(GlobalCommandPaletteFilter::default());
}

/// A filter for the command palette.
#[derive(Default)]
pub struct CommandPaletteFilter {
    hidden_namespaces: HashSet<&'static str>,
    hidden_action_types: TypeIdHashSet,
    /// Actions that have explicitly been shown. These should be shown even if
    /// they are in a hidden namespace.
    shown_action_types: TypeIdHashSet,
}

#[derive(Deref, DerefMut, Default)]
struct GlobalCommandPaletteFilter(CommandPaletteFilter);

impl Global for GlobalCommandPaletteFilter {}

impl CommandPaletteFilter {
    /// Returns the global [`CommandPaletteFilter`], if one is set.
    pub fn try_global(cx: &App) -> Option<&CommandPaletteFilter> {
        cx.try_global::<GlobalCommandPaletteFilter>()
            .map(|filter| &filter.0)
    }

    /// Returns a mutable reference to the global [`CommandPaletteFilter`].
    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<GlobalCommandPaletteFilter>()
    }

    /// Updates the global [`CommandPaletteFilter`] using the given closure.
    pub fn update_global<F>(cx: &mut App, update: F)
    where
        F: FnOnce(&mut Self, &mut App),
    {
        if cx.has_global::<GlobalCommandPaletteFilter>() {
            cx.update_global(|this: &mut GlobalCommandPaletteFilter, cx| update(&mut this.0, cx))
        }
    }

    /// Returns whether the given [`Action`] is hidden by the filter.
    pub fn is_hidden(&self, action: &dyn Action) -> bool {
        let name = action.name();
        let namespace = name.split("::").next().unwrap_or("malformed action name");

        // If this action has specifically been shown then it should be visible.
        if self.shown_action_types.contains(&action.type_id()) {
            return false;
        }

        self.hidden_namespaces.contains(namespace)
            || self.hidden_action_types.contains(&action.type_id())
    }

    /// Hides all actions in the given namespace.
    pub fn hide_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.insert(namespace);
    }

    /// Shows all actions in the given namespace.
    pub fn show_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.remove(namespace);
    }

    /// Hides all actions with the given types.
    pub fn hide_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.hidden_action_types.insert(*action_type);
            self.shown_action_types.remove(action_type);
        }
    }

    /// Shows all actions with the given types.
    pub fn show_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.shown_action_types.insert(*action_type);
            self.hidden_action_types.remove(action_type);
        }
    }
}

/// The result of intercepting a command palette command.
#[derive(Debug)]
pub struct CommandInterceptItem {
    /// The action produced as a result of the interception.
    pub action: Box<dyn Action>,
    /// The display string to show in the command palette for this result.
    pub string: String,
    /// The character positions in the string that match the query.
    /// Used for highlighting matched characters in the command palette UI.
    pub positions: Vec<usize>,
}

/// The result of intercepting a command palette command.
#[derive(Default, Debug)]
pub struct CommandInterceptResult {
    /// The items to show before normal command matches.
    pub results: Vec<CommandInterceptItem>,
    /// The items to show after normal command matches.
    pub append_results: Vec<CommandInterceptItem>,
    /// Whether or not to continue to show the normal matches
    pub exclusive: bool,
}

type CommandPaletteInterceptor =
    Rc<dyn Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>>;

struct DefaultCommandPaletteInterceptor;

/// Interceptors for the command palette.
#[derive(Clone)]
pub struct GlobalCommandPaletteInterceptor(Vec<(TypeId, CommandPaletteInterceptor)>);

impl Global for GlobalCommandPaletteInterceptor {}

impl GlobalCommandPaletteInterceptor {
    /// Sets the default interceptor.
    ///
    /// This will override the previous default interceptor, if it exists.
    pub fn set(
        cx: &mut App,
        interceptor: impl Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>
        + 'static,
    ) {
        Self::set_for_type::<DefaultCommandPaletteInterceptor>(cx, interceptor);
    }

    /// Clears the default interceptor.
    pub fn clear(cx: &mut App) {
        Self::clear_for_type::<DefaultCommandPaletteInterceptor>(cx);
    }

    /// Sets the interceptor for a specific owner type.
    ///
    /// This will override the previous interceptor for the same owner type, if
    /// it exists, without affecting interceptors owned by other integrations.
    pub fn set_for_type<T: 'static>(
        cx: &mut App,
        interceptor: impl Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>
        + 'static,
    ) {
        let type_id = TypeId::of::<T>();
        if cx.has_global::<Self>() {
            cx.update_global(|this: &mut Self, _| {
                this.0
                    .retain(|(existing_type_id, _)| *existing_type_id != type_id);
                this.0.push((type_id, Rc::new(interceptor)));
            });
        } else {
            cx.set_global(Self(vec![(type_id, Rc::new(interceptor))]));
        }
    }

    /// Clears the interceptor for a specific owner type.
    pub fn clear_for_type<T: 'static>(cx: &mut App) {
        let type_id = TypeId::of::<T>();
        if cx.has_global::<Self>() {
            let is_empty = cx.update_global(|this: &mut Self, _| {
                this.0
                    .retain(|(existing_type_id, _)| *existing_type_id != type_id);
                this.0.is_empty()
            });
            if is_empty {
                cx.remove_global::<Self>();
            }
        }
    }

    /// Intercepts the given query from the command palette.
    pub fn intercept(
        query: &str,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Option<Task<CommandInterceptResult>> {
        let interceptors = cx.try_global::<Self>()?.0.clone();
        let query = query.to_owned();
        let tasks = interceptors
            .into_iter()
            .map(|(_, handler)| handler(&query, workspace.clone(), cx))
            .collect::<Vec<_>>();
        Some(cx.spawn(async move |_| {
            let mut result = CommandInterceptResult::default();
            for task in tasks {
                let intercepted = task.await;
                result.results.extend(intercepted.results);
                result.append_results.extend(intercepted.append_results);
                result.exclusive |= intercepted.exclusive;
            }
            result
        }))
    }
}
