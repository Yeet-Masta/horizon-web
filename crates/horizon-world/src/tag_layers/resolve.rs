use super::{Entry, TagLayers};
use std::{borrow::Cow, collections::HashMap};

impl TagLayers {
    /// `TagLoader.build`: resolve one registry's layered tags once, in
    /// dependency order (`DependencySorter.orderByDependencies`, which uses
    /// `isCyclic` to avoid cycle-closing edges). `TagLoader.tryBuildTag`
    /// fails a tag when one of its entries is missing and required
    /// (`TagEntry.build` returns `!required`): a `#tag` that isn't defined or
    /// failed itself, or an element `known` doesn't have
    /// (`ElementLookup.fromFrozenRegistry`). `TagLoader.build` publishes only
    /// the tags that built, so a failed tag resolves to no members here, as
    /// the registry binds it after the reload. A missing optional entry adds
    /// nothing, and a removal never fails its tag. As in `loot/tags.rs`, a
    /// cycle fails every tag on it.
    ///
    /// Graph traversal is O(V + E), plus the work of applying members in
    /// entry order for successfully built tags.
    pub fn resolve_all(
        raw: HashMap<String, Self>,
        known: impl Fn(&str) -> bool,
    ) -> HashMap<String, Vec<String>> {
        let (names, layers): (Vec<_>, Vec<_>) = raw.into_iter().unzip();
        let index: HashMap<_, _> = names
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();
        let dependencies: Vec<Vec<_>> = layers
            .iter()
            .map(|layer| {
                layer
                    .tags()
                    .filter_map(|name| index.get(name).copied())
                    .collect()
            })
            .collect();
        let mut resolved = vec![Vec::<String>::new(); names.len()];
        let mut failed = vec![false; names.len()];

        // Components come out of Kosaraju's second pass in dependents-first order.
        for tags in components(&dependencies).into_iter().rev() {
            let &[tag] = tags.as_slice() else {
                for tag in tags {
                    failed[tag] = true;
                }
                continue;
            };
            let missing: Vec<&str> = layers[tag]
                .0
                .iter()
                .filter(|entry| entry.required && !entry.remove)
                .map(|Entry { id, .. }| id.as_str())
                .filter(|id| match id.strip_prefix('#') {
                    Some(name) => index
                        .get(name)
                        .is_none_or(|&child| child == tag || failed[child]),
                    None => !known(id),
                })
                .collect();
            if !missing.is_empty() {
                log::error!(
                    "Couldn't load tag {} as it is missing following references: {}",
                    names[tag],
                    missing.join(", ")
                );
                failed[tag] = true;
                continue;
            }
            resolved[tag] = layers[tag].build_ordered(
                |name| {
                    let members = index
                        .get(name)
                        .map_or(&[][..], |&id| resolved[id].as_slice());
                    members.iter().map(|id| Cow::Borrowed(id.as_str()))
                },
                &known,
            );
        }

        names.into_iter().zip(resolved).collect()
    }
}

fn components(dependencies: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut dependents = vec![Vec::new(); dependencies.len()];
    for (tag, children) in dependencies.iter().enumerate() {
        for &child in children {
            dependents[child].push(tag);
        }
    }

    let mut seen = vec![false; dependencies.len()];
    let order = postorder(0..dependencies.len(), dependencies, |tag| {
        !std::mem::replace(&mut seen[tag], true)
    });
    seen.fill(false);
    let mut components = Vec::new();
    for root in order.into_iter().rev() {
        if !seen[root] {
            components.push(postorder([root], &dependents, |tag| {
                !std::mem::replace(&mut seen[tag], true)
            }));
        }
    }
    components
}

fn postorder(
    roots: impl IntoIterator<Item = usize>,
    edges: &[Vec<usize>],
    mut enter: impl FnMut(usize) -> bool,
) -> Vec<usize> {
    enum Step {
        Enter(usize),
        Exit(usize),
    }

    let mut order = Vec::new();
    let mut pending = Vec::new();
    for root in roots {
        pending.push(Step::Enter(root));
        while let Some(step) = pending.pop() {
            #[cfg(test)]
            super::work_budget::check_step();
            match step {
                Step::Enter(tag) if enter(tag) => {
                    pending.push(Step::Exit(tag));
                    pending.extend(edges[tag].iter().rev().copied().map(Step::Enter));
                }
                Step::Exit(tag) => order.push(tag),
                Step::Enter(_) => {}
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag_layers::work_budget;
    use serde_json::{Value, json};

    fn layers(values: Value) -> TagLayers {
        TagLayers::from_file(&json!({"values": values}))
    }

    #[test]
    fn cycle_rejects_its_tags_and_dependents_but_keeps_unrelated_tags() {
        let resolved = TagLayers::resolve_all(
            HashMap::from([
                (
                    "minecraft:a".into(),
                    layers(json!(["minecraft:stick", "#b"])),
                ),
                (
                    "minecraft:b".into(),
                    layers(json!(["minecraft:dirt", "#a"])),
                ),
                (
                    "minecraft:c".into(),
                    layers(json!(["minecraft:apple", "#a"])),
                ),
                ("minecraft:d".into(), layers(json!(["minecraft:stone"]))),
                (
                    "minecraft:e".into(),
                    layers(json!(["minecraft:sand", "#c"])),
                ),
            ]),
            |_| true,
        );
        for id in ["minecraft:a", "minecraft:b", "minecraft:c", "minecraft:e"] {
            assert!(resolved[id].is_empty(), "{id}");
        }
        assert_eq!(resolved["minecraft:d"], ["minecraft:stone"]);
    }

    #[test]
    fn self_reference_rejects_its_tag() {
        let resolved = TagLayers::resolve_all(
            HashMap::from([(
                "minecraft:a".into(),
                layers(json!(["minecraft:stick", "#a"])),
            )]),
            |_| true,
        );
        assert!(resolved["minecraft:a"].is_empty());
    }

    #[test]
    fn optional_missing_and_empty_references_contribute_no_members() {
        let resolved = TagLayers::resolve_all(
            HashMap::from([
                ("minecraft:empty".into(), layers(json!([]))),
                (
                    "minecraft:a".into(),
                    layers(json!([
                        "stick",
                        {"id": "#unknown", "required": false},
                        {"id": "not_an_item", "required": false},
                        "#empty",
                        "apple"
                    ])),
                ),
            ]),
            |id| id != "minecraft:not_an_item",
        );
        assert_eq!(
            resolved["minecraft:a"],
            ["minecraft:stick", "minecraft:apple"]
        );
    }

    #[test]
    fn missing_required_reference_fails_the_tag_and_tags_requiring_it() {
        let mut removes_missing = layers(json!(["stone"]));
        removes_missing.push(&json!({"values": [], "remove": ["not_a_block", "#typo"]}));
        let resolved = TagLayers::resolve_all(
            HashMap::from([
                (
                    "minecraft:mineable/pickaxe".into(),
                    layers(json!(["stone", "not_a_block"])),
                ),
                (
                    "minecraft:typo_ref".into(),
                    layers(json!(["dirt", "#typo"])),
                ),
                (
                    "minecraft:requires_failed".into(),
                    layers(json!(["dirt", "#mineable/pickaxe"])),
                ),
                (
                    "minecraft:optional_failed".into(),
                    layers(json!([
                        "dirt",
                        {"id": "#mineable/pickaxe", "required": false}
                    ])),
                ),
                ("minecraft:removes_missing".into(), removes_missing),
            ]),
            |id| id != "minecraft:not_a_block",
        );
        for id in [
            "minecraft:mineable/pickaxe",
            "minecraft:typo_ref",
            "minecraft:requires_failed",
        ] {
            assert!(resolved[id].is_empty(), "{id}");
        }
        assert_eq!(resolved["minecraft:optional_failed"], ["minecraft:dirt"]);
        assert_eq!(resolved["minecraft:removes_missing"], ["minecraft:stone"]);
    }

    #[test]
    fn dense_cycle_has_linear_traversal_work() {
        const TAGS: usize = 128;
        let tags = (0..TAGS)
            .map(|tag| {
                let references: Vec<_> = (0..TAGS)
                    .filter(|&other| other != tag)
                    .map(|other| format!("#test:cycle/{other}"))
                    .chain(["stick".into()])
                    .collect();
                (format!("test:cycle/{tag}"), layers(json!(references)))
            })
            .collect();

        let resolved =
            work_budget::with_limit(4 * TAGS * TAGS, || TagLayers::resolve_all(tags, |_| true));
        assert_eq!(resolved.len(), TAGS);
        assert!(resolved.values().all(Vec::is_empty));
    }

    #[test]
    fn deep_diamond_dag_has_linear_traversal_work() {
        const DEPTH: usize = 40;
        let mut tags = HashMap::new();
        for depth in 0..DEPTH {
            for side in ["left", "right"] {
                tags.insert(
                    format!("test:{side}/{depth}"),
                    layers(json!([format!("#test:join/{}", depth + 1)])),
                );
            }
            tags.insert(
                format!("test:join/{depth}"),
                layers(json!([
                    format!("#test:left/{depth}"),
                    format!("#test:right/{depth}")
                ])),
            );
        }
        tags.insert(
            format!("test:join/{DEPTH}"),
            layers(json!(["stick", "dirt"])),
        );

        let resolved = work_budget::with_limit(4 * (tags.len() + 4 * DEPTH), || {
            TagLayers::resolve_all(tags, |_| true)
        });
        assert_eq!(
            resolved["test:join/0"],
            ["minecraft:stick", "minecraft:dirt"]
        );
    }
}
