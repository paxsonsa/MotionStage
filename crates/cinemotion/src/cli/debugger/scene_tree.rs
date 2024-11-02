use cinemotion_core as core;

use ratatui::{
    buffer::Buffer,
    widgets::{Block, StatefulWidget},
};
use tui_tree_widget::{Tree, TreeItem, TreeState};

pub(crate) struct SceneGraphWidget<'t> {
    items: Vec<TreeItem<'t, String>>,
}

impl<'t> SceneGraphWidget<'t> {
    pub fn build(
        scene_state: &'t cinemotion_core::state::GlobalState,
        tree_state: &mut TreeState<String>,
    ) -> Self {
        let mut items = vec![];

        let node_id = "devices".to_string();
        let mut root = TreeItem::new(node_id.clone(), "Devices".to_string(), vec![])
            .expect("failed to create root device node.");
        tree_state.open(vec![node_id]);

        for (id, device) in scene_state.devices.iter() {
            let id = format!("devices:{}", id);
            let mut node = TreeItem::new(id.clone(), device.name.to_string(), vec![])
                .expect("failed to create device node");

            for (key, attr) in device.attributes.iter() {
                let node_id = build_tree_id(&id, &key);
                let mut leaf = TreeItem::new(node_id.clone(), format!("{}", key), vec![])
                    .expect("failed to create device attribute node");

                add_attribute_nodes(&mut leaf, &id, &node_id, attr, tree_state);

                node.add_child(leaf)
                    .expect("failed to add device attribute node");
                tree_state.open(vec!["devices".to_string(), id.clone(), node_id]);
            }

            root.add_child(node).expect("failed to add device node");
            tree_state.open(vec!["devices".to_string(), id.clone()]);
        }

        items.push(root);
        Self { items }
    }
}

impl<'t> StatefulWidget for SceneGraphWidget<'t> {
    type State = TreeState<String>;

    fn render(self, area: ratatui::prelude::Rect, buf: &mut Buffer, state: &mut Self::State) {
        let tree = Tree::new(&self.items)
            .expect("tree did not render")
            .block(Block::bordered().title("Scene State"));
        StatefulWidget::render(tree, area, buf, state);
    }
}

fn build_tree_id<T>(id: &str, key: &T) -> String
where
    T: std::fmt::Display,
{
    format!("{}:{}", id, key)
}

fn add_attribute_nodes<'a>(
    leaf: &mut TreeItem<'a, String>,
    id: &str,
    node_id: &str,
    attr: &core::prelude::Attribute,
    tree_state: &mut TreeState<String>,
) {
    match &*attr.value() {
        core::prelude::AttributeValue::Float(f) => {
            add_float_node(leaf, id, node_id, f, tree_state);
        }
        core::prelude::AttributeValue::Vec3(vec3) => {
            add_vec3_node(leaf, id, node_id, vec3, tree_state);
        }
        core::prelude::AttributeValue::Vec4(vec4) => {
            add_vec4_node(leaf, id, node_id, vec4, tree_state);
        }
        core::prelude::AttributeValue::Matrix44(matrix44) => {
            add_matrix44_node(leaf, id, node_id, matrix44, tree_state);
        }
    }
}

fn add_float_node<'a>(
    leaf: &mut TreeItem<'a, String>,
    id: &str,
    node_id: &str,
    value: &f64,
    tree_state: &mut TreeState<String>,
) {
    let value_id = build_tree_id(node_id, &0);
    let value_node = TreeItem::new_leaf(value_id.clone(), format!("{}", value));
    leaf.add_child(value_node)
        .expect("failed to add value node");
    tree_state.open(vec![id.to_string(), node_id.to_string(), value_id]);
}

fn add_vec3_node<'a>(
    leaf: &mut TreeItem<'a, String>,
    id: &str,
    node_id: &str,
    vec3: &core::prelude::Vec3,
    tree_state: &mut TreeState<String>,
) {
    for (i, value) in [vec3.x, vec3.y, vec3.z].iter().enumerate() {
        let value_id = build_tree_id(node_id, &i);
        let value_node = TreeItem::new_leaf(value_id.clone(), format!("{}", value));
        leaf.add_child(value_node)
            .expect("failed to add value node");
        tree_state.open(vec![id.to_string(), node_id.to_string(), value_id]);
    }
}

fn add_vec4_node<'a>(
    leaf: &mut TreeItem<'a, String>,
    id: &str,
    node_id: &str,
    vec4: &core::prelude::Vec4,
    tree_state: &mut TreeState<String>,
) {
    for (i, value) in [vec4.x, vec4.y, vec4.z, vec4.w].iter().enumerate() {
        let value_id = build_tree_id(node_id, &i);
        let value_node = TreeItem::new_leaf(value_id.clone(), format!("{}", value));
        leaf.add_child(value_node)
            .expect("failed to add value node");
        tree_state.open(vec![id.to_string(), node_id.to_string(), value_id]);
    }
}

fn add_matrix44_node<'a>(
    leaf: &mut TreeItem<'a, String>,
    id: &str,
    node_id: &str,
    matrix44: &core::prelude::Matrix44,
    tree_state: &mut TreeState<String>,
) {
    for (i, row) in matrix44.iter_rows().enumerate() {
        let value_id = build_tree_id(node_id, &i);
        let value_node = TreeItem::new_leaf(value_id.clone(), format!("{:?}", row));
        leaf.add_child(value_node)
            .expect("failed to add value to node");
        tree_state.open(vec![id.to_string(), node_id.to_string(), value_id]);
    }
}
