//! Outputs and their workspaces: which window lives where, and in what role.

use smithay::{desktop::Window, output::Output};

use crate::{
    config::{Config, WORKSPACES},
    layout::{Dwindle, Rect},
};

/// How a window covers its workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullKind {
    /// The whole output, without gap or border.
    Fullscreen,
    /// The output inside the outer gap.
    Maximized,
}

/// A window outside the tiling tree.
#[derive(Debug, Clone)]
pub struct Floating {
    pub window: Window,
    /// Position and the size asked of the client. Width 0 means the client has not picked one.
    pub rect: Rect,
    /// Right and bottom edge to hold still while a resize moves the opposite one.
    pub anchor: (Option<i32>, Option<i32>),
}

impl Floating {
    pub fn new(window: Window, rect: Rect) -> Self {
        Self {
            window,
            rect,
            anchor: (None, None),
        }
    }

    /// Where the window really is: clients have the last word on their size.
    pub fn content(&self) -> Rect {
        let size = self.window.geometry().size;
        let (w, h) = if size.w > 0 && size.h > 0 {
            (size.w, size.h)
        } else {
            (self.rect.w, self.rect.h)
        };
        let x = self.anchor.0.map_or(self.rect.x, |right| right - w);
        let y = self.anchor.1.map_or(self.rect.y, |bottom| bottom - h);
        Rect::new(x, y, w, h)
    }
}

/// What a window is on its workspace, apart from fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Tiled,
    Floating(Rect),
}

/// What a window should be told in its next configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub size: Option<(i32, i32)>,
    pub full: Option<FullKind>,
}

/// A visible window and the space it takes up.
#[derive(Debug, Clone)]
pub struct Frame {
    pub window: Window,
    /// Including the border.
    pub outer: Rect,
    /// The window content.
    pub inner: Rect,
    pub has_border: bool,
    pub tiled: bool,
    pub request: Request,
}

#[derive(Debug, Default)]
pub struct Workspace {
    pub tiled: Dwindle<Window>,
    /// Bottom to top.
    pub floating: Vec<Floating>,
    /// The window covering this workspace. The others stay where they are, but hidden.
    pub full: Option<(Window, FullKind)>,
    /// Who gets the keyboard when this workspace is entered.
    pub focused: Option<Window>,
}

impl Workspace {
    /// All windows, tiled ones first, floating ones bottom to top.
    pub fn windows(&self) -> Vec<Window> {
        let tiled = self.tiled.items().into_iter().cloned();
        tiled
            .chain(self.floating.iter().map(|entry| entry.window.clone()))
            .collect()
    }

    pub fn contains(&self, window: &Window) -> bool {
        self.tiled.contains(window) || self.floating_entry(window).is_some()
    }

    pub fn floating_entry(&self, window: &Window) -> Option<&Floating> {
        self.floating.iter().find(|entry| entry.window == *window)
    }

    pub fn floating_entry_mut(&mut self, window: &Window) -> Option<&mut Floating> {
        self.floating
            .iter_mut()
            .find(|entry| entry.window == *window)
    }

    pub fn placement(&self, window: &Window) -> Option<Placement> {
        match self.floating_entry(window) {
            Some(entry) => Some(Placement::Floating(entry.rect)),
            None => self.tiled.contains(window).then_some(Placement::Tiled),
        }
    }

    /// Puts a floating window on top of the others. Returns whether anything changed.
    pub fn raise(&mut self, window: &Window) -> bool {
        let Some(index) = self
            .floating
            .iter()
            .position(|entry| entry.window == *window)
        else {
            return false;
        };
        let entry = self.floating.remove(index);
        self.floating.push(entry);
        index + 1 != self.floating.len()
    }

    /// The window that should get the keyboard, if nobody else says otherwise.
    pub fn focus_candidate(&self) -> Option<Window> {
        self.full
            .as_ref()
            .map(|(window, _)| window)
            .or(self.focused.as_ref())
            .or(self.floating.last().map(|entry| &entry.window))
            .or(self.tiled.items().last().copied())
            .cloned()
    }

    /// The window that takes over from `window` when it leaves.
    fn successor(&self, window: &Window) -> Option<Window> {
        self.tiled
            .neighbour(window)
            .or_else(|| {
                let others = self.floating.iter().map(|entry| &entry.window);
                others.rev().find(|other| *other != window)
            })
            .or_else(|| {
                let others = self.tiled.items();
                others
                    .iter()
                    .rev()
                    .find(|other| ***other != *window)
                    .copied()
            })
            .cloned()
    }

    /// Takes `window` off this workspace. Returns what it was.
    pub fn take(&mut self, window: &Window) -> Option<Placement> {
        let placement = self.placement(window)?;
        let successor = self.successor(window);
        self.tiled.remove(window);
        self.floating.retain(|entry| entry.window != *window);
        if self.full.as_ref().is_some_and(|(full, _)| full == window) {
            self.full = None;
        }
        if self.focused.as_ref() == Some(window) {
            self.focused = successor;
        }
        Some(placement)
    }

    /// Adds `window` next to the focused window.
    pub fn put(&mut self, window: Window, placement: Placement) {
        match placement {
            Placement::Tiled => self.tiled.insert(self.focused.as_ref(), window),
            Placement::Floating(rect) => self.floating.push(Floating::new(window, rect)),
        }
    }

    /// The visible windows on an output of the size `area`, bottom to top. Fullscreen covers all
    /// of it, everything else stays inside `work`, what panels leave free.
    pub fn frames(&self, area: Rect, work: Rect, config: &Config) -> Vec<Frame> {
        let border = config.border_width;
        if let Some((window, kind)) = &self.full {
            let outer = match kind {
                FullKind::Fullscreen => area,
                FullKind::Maximized => work.shrink(config.gaps.outer),
            };
            let has_border = *kind == FullKind::Maximized;
            let inner = if has_border {
                outer.shrink(border)
            } else {
                outer
            };
            return vec![Frame {
                window: window.clone(),
                outer,
                inner,
                has_border,
                tiled: false,
                request: Request {
                    size: Some((inner.w, inner.h)),
                    full: Some(*kind),
                },
            }];
        }

        let tiles = self.tiled.arrange(work, config.gaps);
        let tiles = tiles.into_iter().map(|(window, outer)| {
            let inner = outer.shrink(border);
            Frame {
                window: window.clone(),
                outer,
                inner,
                has_border: true,
                tiled: true,
                request: Request {
                    size: Some((inner.w, inner.h)),
                    full: None,
                },
            }
        });
        let floating = self.floating.iter().map(|entry| {
            let inner = entry.content();
            Frame {
                window: entry.window.clone(),
                outer: inner.expand(border),
                inner,
                has_border: true,
                tiled: false,
                request: Request {
                    size: (entry.rect.w > 0).then_some((entry.rect.w, entry.rect.h)),
                    full: None,
                },
            }
        });
        tiles.chain(floating).collect()
    }
}

pub struct Monitor {
    pub output: Output,
    /// A disconnected monitor keeps the windows of the last output until the next one appears.
    pub connected: bool,
    pub workspaces: Vec<Workspace>,
    /// Index into `workspaces`.
    pub active: usize,
    /// What `work_area` said at the last `retile`.
    pub work_area: Option<Rect>,
}

impl Monitor {
    pub fn new(output: Output) -> Self {
        Self {
            output,
            connected: true,
            workspaces: (0..WORKSPACES).map(|_| Workspace::default()).collect(),
            active: 0,
            work_area: None,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspaces[self.active]
    }

    pub fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active]
    }
}
