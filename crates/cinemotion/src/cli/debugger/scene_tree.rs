use ratatui::{buffer::Buffer, widgets::StatefulWidget};
use tui_tree_widget::{Tree, TreeItem, TreeState};

pub(crate) struct SceneGraphWidget<'t> {
    items: Vec<TreeItem<'t, String>>,
}

impl<'t> SceneGraphWidget<'t> {
    pub fn new(scene_state: &'t cinemotion_core::state::GlobalState) -> Self {
        let items = vec![];
        let mut root = TreeItem::new("devices".to_string(), "Devices".to_string(), vec![])
            .expect("failed to create root device node.");
        for (id, device) in scene_state.devices.iter() {
            root.add_child(
                TreeItem::new(format!("devices:{}", id), device.name.to_string(), vec![])
                    .expect("failed to create device node"),
            )
            .expect("failed to add device node");
        }

        Self { items }
    }
}

impl<'t> StatefulWidget for SceneGraphWidget<'t> {
    type State = TreeState<String>;

    fn render(self, area: ratatui::prelude::Rect, buf: &mut Buffer, state: &mut Self::State) {
        let tree = Tree::new(&self.items).expect("tree did not render");
        StatefulWidget::render(tree, area, buf, state);
    }
}
