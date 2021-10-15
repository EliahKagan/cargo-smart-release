use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    iter::FromIterator,
    path::PathBuf,
    time::Instant,
};

use anyhow::bail;
use cargo_metadata::Package;
use git_repository as git;
use git_repository::{
    bstr::ByteSlice,
    easy::head,
    prelude::{CacheAccessExt, ObjectAccessExt, ObjectIdExt, ReferenceAccessExt, ReferenceExt},
};

use crate::{
    commit,
    commit::history::{Item, Segment},
    git::strip_tag_path,
    utils::{component_to_bytes, is_tag_name, is_tag_version, tag_prefix},
    Context,
};

pub enum SegmentScope {
    /// Stop finding segments after the unreleased/first section was processed.
    Unreleased,
    /// Obtain all segments, including unreleased and tags
    EntireHistory,
}

pub fn collect(repo: &git::Easy) -> anyhow::Result<Option<commit::History>> {
    let start = Instant::now();
    let prev = repo.object_cache_size(64 * 1024)?;
    let reference = match repo.head()?.peeled()?.kind {
        head::Kind::Detached { .. } => bail!("Refusing to operate on a detached head."),
        head::Kind::Unborn { .. } => return Ok(None),
        head::Kind::Symbolic(r) => r.attach(repo),
    };

    let mut items = Vec::new();
    let mut data_by_tree_id = HashMap::default();
    for commit_id in reference.id().ancestors()?.all() {
        let commit_id = commit_id?;
        let (message, tree_id, parent_tree_id, commit_time) = {
            let (message, tree_id, commit_time, parent_commit_id) = {
                let object = commit_id.object()?;
                let commit = object.to_commit();
                (
                    commit.message.to_vec(),
                    commit.tree(),
                    commit.committer.time,
                    commit.parents().last(),
                )
            };
            (
                message,
                tree_id,
                parent_commit_id.map(|id| id.attach(repo).object().expect("present").to_commit().tree()),
                commit_time,
            )
        };

        let message = match message.to_str() {
            Err(_) => {
                log::warn!(
                    "Commit message of {} could not be decoded to UTF-8 - ignored",
                    commit_id.as_ref()
                );
                continue;
            }
            Ok(m) => m,
        };
        data_by_tree_id.insert(tree_id, repo.find_object(tree_id)?.data.to_owned());
        if let Some(tree_id) = parent_tree_id {
            data_by_tree_id.insert(tree_id, repo.find_object(tree_id)?.data.to_owned());
        }
        items.push(commit::history::Item {
            id: commit_id.detach(),
            commit_time,
            message: commit::Message::from(message),
            tree_id,
            parent_tree_id,
        });
    }
    repo.object_cache_size(prev)?;
    items.sort_by(|lhs, rhs| {
        (lhs.commit_time.time as i64 + lhs.commit_time.offset as i64)
            .cmp(&(rhs.commit_time.time as i64 + rhs.commit_time.offset as i64))
            .reverse()
    });

    let elapsed = start.elapsed();
    log::trace!(
        "Cached commit history of {} commits and trees in {}s ({:.0} items/s)",
        items.len(),
        elapsed.as_secs_f32(),
        items.len() as f32 / elapsed.as_secs_f32()
    );
    Ok(Some(commit::History {
        head: reference.detach(),
        items,
        data_by_tree_id,
    }))
}

/// Return the head reference followed by all tags affecting `crate_name` as per our tag name rules, ordered by ancestry.
pub fn crate_ref_segments<'h>(
    package: &Package,
    ctx: &crate::Context,
    history: &'h commit::History,
    scope: SegmentScope,
) -> anyhow::Result<Vec<commit::history::Segment<'h>>> {
    let tag_prefix = tag_prefix(package, &ctx.repo);
    let start = Instant::now();
    let mut tags_by_commit = {
        let refs = ctx.repo.references()?;
        match tag_prefix {
            Some(prefix) => BTreeMap::from_iter(
                refs.prefixed(PathBuf::from(format!("refs/tags/{}-", prefix)))?
                    .peeled()
                    .filter_map(|r| r.ok().map(|r| r.detach()))
                    .filter(|r| is_tag_name(prefix, strip_tag_path(r.name.to_ref())))
                    .map(|r| {
                        let t = r.peeled.expect("already peeled");
                        (t, r)
                    }),
            ),
            None => BTreeMap::from_iter(
                refs.prefixed("refs/tags")?
                    .peeled()
                    .filter_map(|r| r.ok().map(|r| r.detach()))
                    .filter(|r| is_tag_version(strip_tag_path(r.name.to_ref())))
                    .map(|r| {
                        let t = r.peeled.expect("already peeled");
                        (t, r)
                    }),
            ),
        }
    };

    let elapsed = start.elapsed();
    log::trace!(
        "{}: Mapped {} tags in {}s ({:.0} refs/s)",
        package.name,
        tags_by_commit.len(),
        elapsed.as_secs_f32(),
        tags_by_commit.len() as f32 / elapsed.as_secs_f32()
    );

    let start = Instant::now();
    let mut segments = Vec::new();
    let mut segment = commit::history::Segment {
        head: history.head.to_owned(),
        history: vec![],
    };

    let dir = ctx.repo_relative_path(package);
    let filter = dir
        .map(|dir| {
            let mut components = dir.components().collect::<Vec<_>>();
            match components.len() {
                0 => unreachable!("BUG: it's None if empty"),
                1 => Filter::Fast(components.pop().map(component_to_bytes).expect("exactly one")),
                _ => Filter::Slow(components.into_iter().map(component_to_bytes).collect()),
            }
        })
        .unwrap_or_else(|| {
            if ctx.meta.workspace_members.len() == 1 {
                Filter::None
            } else {
                log::info!(
                    "{}: Tracking top-level crate's changes in multi-crate workspace through 'src/' directory only.",
                    package.name
                );
                // TODO: analyse .targets to find actual source directory.
                Filter::Fast(b"src")
            }
        });

    for item in history.items.iter() {
        match tags_by_commit.remove(&item.id) {
            None => add_item_if_package_changed(ctx, &mut segment, &filter, item, &history.data_by_tree_id)?,
            Some(next_ref) => {
                match scope {
                    SegmentScope::EntireHistory => {
                        segments.push(std::mem::replace(
                            &mut segment,
                            commit::history::Segment {
                                head: next_ref,
                                history: vec![],
                            },
                        ));
                    }
                    SegmentScope::Unreleased => {
                        segments.push(segment);
                        return Ok(segments);
                    }
                }
                add_item_if_package_changed(ctx, &mut segment, &filter, item, &history.data_by_tree_id)?
            }
        }
    }
    segments.push(segment);

    if matches!(scope, SegmentScope::EntireHistory) && !tags_by_commit.is_empty() {
        log::warn!(
            "{}: The following tags were not encountered during commit graph traversal: {}",
            package.name,
            tags_by_commit
                .into_values()
                .map(|v| v.name.as_bstr().to_str_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    let elapsed = start.elapsed();
    let num_commits = segments.iter().map(|s| s.history.len()).sum::<usize>();
    log::trace!(
        "{}: Found {} relevant commits out of {} in {} segments {}s ({:.0} commits/s)",
        package.name,
        num_commits,
        history.items.len(),
        segments.len(),
        elapsed.as_secs_f32(),
        num_commits as f32 / elapsed.as_secs_f32()
    );

    Ok(segments)
}

enum Filter<'a> {
    None,
    Fast(&'a [u8]),
    Slow(Vec<&'a [u8]>),
}

fn add_item_if_package_changed<'a>(
    ctx: &Context,
    segment: &mut Segment<'a>,
    filter: &Filter<'_>,
    item: &'a Item,
    data_by_tree_id: &HashMap<git::ObjectId, Vec<u8>>,
) -> anyhow::Result<()> {
    let history = &mut segment.history;
    match filter {
        Filter::None => history.push(item),
        Filter::Fast(comp) => {
            let current = git::objs::TreeRefIter::from_bytes(&data_by_tree_id[&item.tree_id])
                .filter_map(Result::ok)
                .find(|e| e.filename == comp);
            let parent = item.parent_tree_id.and_then(|parent| {
                git::objs::TreeRefIter::from_bytes(&data_by_tree_id[&parent])
                    .filter_map(Result::ok)
                    .find(|e| e.filename == comp)
            });
            match (current, parent) {
                (Some(current), Some(parent)) => {
                    if current.oid != parent.oid {
                        history.push(item)
                    }
                }
                (Some(_), None) => history.push(item),
                (None, Some(_)) | (None, None) => {}
            };
        }
        Filter::Slow(ref components) => {
            let prev = ctx.repo.object_cache_size(1024 * 1024)?;
            let current_data = RefCell::new(item.tree_id);
            let current = git::easy::TreeRef::from_id_and_data(
                item.id,
                std::cell::Ref::map(current_data.borrow(), |v| v.as_slice()),
                &ctx.repo,
            )
            .lookup_path(components.iter().copied())?;
            let parent = match item.parent_tree_id {
                Some(tree_id) => {
                    let parent_data = RefCell::new(data_by_tree_id[&tree_id].to_owned());
                    git::easy::TreeRef::from_id_and_data(
                        tree_id,
                        std::cell::Ref::map(parent_data.borrow(), |v| v.as_slice()),
                        &ctx.repo,
                    )
                    .lookup_path(components.iter().copied())?
                }
                None => None,
            };
            match (current, parent) {
                (Some(current), Some(parent)) => {
                    if current.oid != parent.oid {
                        history.push(item)
                    }
                }
                (Some(_), None) => history.push(item),
                (None, Some(_)) | (None, None) => {}
            };
            ctx.repo.object_cache_size(prev)?;
        }
    };
    Ok(())
}
