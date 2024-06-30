use std::cmp::min;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use chrono::{prelude::*, Duration};
use common::config::LFSConfig;
use jupiter::storage::lfs_storage::LfsStorage;
use rand::prelude::*;

use callisto::{lfs_locks, lfs_objects};
use common::errors::{GitLFSError, MegaError};

use crate::lfs::lfs_structs::{
    BatchRequest, LockList, LockRequest, ObjectError, UnlockRequest, VerifiableLockList,
    VerifiableLockRequest,
};
use crate::lfs::lfs_structs::{Link, Lock, LockListQuery, MetaObject, Representation, RequestVars};
use crate::lfs::LfsConfig;

pub async fn lfs_retrieve_lock(
    config: &LfsConfig,
    query: LockListQuery,
) -> Result<LockList, GitLFSError> {
    let mut lock_list = LockList {
        locks: vec![],
        next_cursor: "".to_string(),
    };
    match lfs_get_filtered_locks(
        config.context.services.lfs_storage.clone(),
        &query.refspec,
        &query.path,
        &query.cursor,
        &query.limit,
    )
    .await
    {
        Ok((locks, next)) => {
            lock_list.locks = locks;
            lock_list.next_cursor = next;
            Ok(lock_list)
        }
        Err(_) => Err(GitLFSError::GeneralError(
            "Lookup operation failed!".to_string(),
        )),
    }
}

pub async fn lfs_verify_lock(
    config: &LfsConfig,
    req: VerifiableLockRequest,
) -> Result<VerifiableLockList, MegaError> {
    let mut limit = req.limit.unwrap_or(0);
    if limit == 0 {
        limit = 100;
    }
    let res = lfs_get_filtered_locks(
        config.context.services.lfs_storage.clone(),
        &req.refs.name,
        "",
        &req.cursor.clone().unwrap_or("".to_string()).to_string(),
        &limit.to_string(),
    )
    .await;

    let mut lock_list = VerifiableLockList {
        ours: vec![],
        theirs: vec![],
        next_cursor: "".to_string(),
    };
    match res {
        Ok((locks, next_cursor)) => {
            lock_list.next_cursor = next_cursor;

            for lock in locks.iter() {
                if Option::is_none(&lock.owner) {
                    lock_list.ours.push(lock.clone());
                } else {
                    lock_list.theirs.push(lock.clone());
                }
            }
        }
        Err(_) => return Err(MegaError::with_message("Lookup operation failed!")),
    };
    Ok(lock_list)
}

pub async fn lfs_create_lock(config: &LfsConfig, req: LockRequest) -> Result<Lock, GitLFSError> {
    let res = lfs_get_filtered_locks(
        config.context.services.lfs_storage.clone(),
        &req.refs.name,
        &req.path.to_string(),
        "",
        "1",
    )
    .await;

    match res {
        Ok((locks, _)) => {
            if !locks.is_empty() {
                return Err(GitLFSError::GeneralError("Lock already exist".to_string()));
            }
        }
        Err(_) => {
            return Err(GitLFSError::GeneralError(
                "Failed when filtering locks!".to_string(),
            ));
        }
    };

    let lock = Lock {
        id: {
            let mut random_num = String::new();
            let mut rng = rand::thread_rng();
            for _ in 0..8 {
                random_num += &(rng.gen_range(0..9)).to_string();
            }
            random_num
        },
        path: req.path.to_owned(),
        owner: None,
        locked_at: {
            let locked_at: DateTime<Utc> = Utc::now();
            locked_at.to_rfc3339()
        },
    };

    match lfs_add_lock(
        config.context.services.lfs_storage.clone(),
        &req.refs.name,
        vec![lock.clone()],
    )
    .await
    {
        Ok(_) => Ok(lock),
        Err(_) => Err(GitLFSError::GeneralError(
            "Failed when adding locks!".to_string(),
        )),
    }
}

pub async fn lfs_delete_lock(
    config: &LfsConfig,
    id: &str,
    unlock_request: UnlockRequest,
) -> Result<Lock, GitLFSError> {
    if id.is_empty() {
        return Err(GitLFSError::GeneralError("Invalid lock id!".to_string()));
    }
    let res = delete_lock(
        config.context.services.lfs_storage.clone(),
        &unlock_request.refs.name,
        None,
        id,
        unlock_request.force.unwrap_or(false),
    )
    .await;
    match res {
        Ok(deleted_lock) => {
            if deleted_lock.id.is_empty()
                && deleted_lock.path.is_empty()
                && deleted_lock.owner.is_none()
                && deleted_lock.locked_at == DateTime::<Utc>::MIN_UTC.to_rfc3339()
            {
                Err(GitLFSError::GeneralError(
                    "Unable to find lock!".to_string(),
                ))
            } else {
                Ok(deleted_lock)
            }
        }
        Err(_) => Err(GitLFSError::GeneralError(
            "Delete operation failed!".to_string(),
        )),
    }
}

/// Process batch request.
/// if operation is "download":
///     1. if server enable splite and client support it, then return a list of links to download the object.
///     2. if server enable splite and client not support it, then return a single link to download the object.
///     3. if server not enable splite, client support it, report error.
/// if operation is "upload", then return a link to upload the object.
pub async fn lfs_process_batch(
    config: &LfsConfig,
    mut batch_vars: BatchRequest,
) -> Result<Vec<Representation>, GitLFSError> {
    let bvo = &mut batch_vars.objects;
    for request in bvo {
        request.authorization = "".to_string();
    }
    let mut response_objects = Vec::<Representation>::new();
    let server_url = format!("http://{}:{}", config.host, config.port);

    let storage = config.context.services.lfs_storage.clone();
    
    for object in &batch_vars.objects {
        let meta = lfs_get_meta(storage.clone(), object).await;
        // Found
        let found = meta.is_ok();
        let mut meta = meta.unwrap_or_default();
        if found && config.lfs_storage.exist_object(&config.repo_name, &meta.oid) {
            response_objects.push(represent(object, &meta, true, false, false, &server_url).await);
            continue;
        }
        // Not found
        if batch_vars.operation == "upload" {
            meta = lfs_put_meta(storage.clone(), object, config.enable_split).await.unwrap();
            response_objects.push(represent(object, &meta, false, true, false, &server_url).await);
        } else {
            let rep = Representation {
                oid: object.oid.to_owned(),
                size: object.size,
                authenticated: None,
                actions: None,
                error: Some(ObjectError {
                    code: 404,
                    message: "Not found".to_owned(),
                }),
            };
            response_objects.push(rep);
        }
    }
    Ok(response_objects)
}

/// if server enable split, then return a list of chunk ids.
/// else return an error.
pub async fn lfs_fetch_chunk_ids(config: &LFSConfig, oid: &str) -> Result<(), GitLFSError> {
    unimplemented!();
}

/// Upload object to storage.
/// if server enable split, split the object and upload each part to storage, save the relationship to database.
pub async fn lfs_upload_object(
    config: &LfsConfig,
    request_vars: &RequestVars,
    body_bytes: &[u8],
) -> Result<(), GitLFSError> {
    let meta = lfs_get_meta(config.context.services.lfs_storage.clone(), request_vars)
        .await
        .unwrap();
    // TODO: splite
    let res = config
        .lfs_storage
        .put_object(&config.repo_name,&meta.oid,  body_bytes)
        .await;
    if res.is_err() {
        lfs_delete_meta(config.context.services.lfs_storage.clone(), request_vars)
            .await
            .unwrap();
        return Err(GitLFSError::GeneralError(String::from(
            "Header not acceptable!",
        )));
    }
    Ok(())
}

/// Download object from storage.
/// when server enable split,  if OID is a complete object, then splice the object and return it.
pub async fn lfs_download_object(
    config: &LfsConfig,
    request_vars: &RequestVars,
) -> Result<Bytes, GitLFSError> {
    let meta = lfs_get_meta(config.context.services.lfs_storage.clone(), request_vars)
        .await
        .unwrap();
    let bytes = config.lfs_storage.get_object(&config.repo_name,&meta.oid).await.unwrap();
    Ok(bytes)
}

pub async fn represent(
    rv: &RequestVars,
    meta: &MetaObject,
    download: bool,
    upload: bool,
    use_tus: bool,
    server_url: &str,
) -> Representation {
    let mut rep = Representation {
        oid: meta.oid.to_owned(),
        size: meta.size,
        authenticated: Some(true),
        actions: None,
        error: None,
    };

    let header = {
        let mut header = HashMap::new();
        header.insert("Accept".to_string(), "application/vnd.git-lfs".to_owned());
        if !rv.authorization.is_empty() {
            header.insert("Authorization".to_string(), rv.authorization.to_owned());
        }
        header
    };

    let mut actions = HashMap::new();
    if download {
        actions.insert(
            "download".to_string(),
            create_link(&rv.download_link(server_url.to_string()).await, &header),
        );
    }

    if upload {
        actions.insert(
            "upload".to_string(),
            create_link(&rv.upload_link(server_url.to_string()).await, &header),
        );

        if use_tus {
            actions.insert(
                "verify".to_string(),
                create_link(&rv.verify_link(server_url.to_string()).await, &header),
            );
        }
    }

    if !actions.is_empty() {
        rep.actions = Some(actions);
    }

    rep
}

fn create_link(href: &str, header: &HashMap<String, String>) -> Link {
    Link {
        href: href.to_string(),
        header: header.clone(),
        expires_at: {
            let expire_time: DateTime<Utc> = Utc::now() + Duration::try_seconds(86400).unwrap();
            expire_time.to_rfc3339()
        },
    }
}

async fn lfs_get_filtered_locks(
    storage: Arc<LfsStorage>,
    refspec: &str,
    path: &str,
    cursor: &str,
    limit: &str,
) -> Result<(Vec<Lock>, String), GitLFSError> {
    let mut locks = match lfs_get_locks(storage, refspec).await {
        Ok(locks) => locks,
        Err(_) => vec![],
    };

    tracing::debug!("Locks retrieved: {:?}", locks);

    if !cursor.is_empty() {
        let mut last_seen = -1;
        for (i, v) in locks.iter().enumerate() {
            if v.id == *cursor {
                last_seen = i as i32;
                break;
            }
        }

        if last_seen > -1 {
            locks = locks.split_off(last_seen as usize);
        } else {
            // Cursor not found.
            return Err(GitLFSError::GeneralError("".to_string()));
        }
    }

    if !path.is_empty() {
        let mut filterd = Vec::<Lock>::new();
        for lock in locks.iter() {
            if lock.path == *path {
                filterd.push(Lock {
                    id: lock.id.to_owned(),
                    path: lock.path.to_owned(),
                    owner: lock.owner.clone(),
                    locked_at: lock.locked_at.to_owned(),
                });
            }
        }
        locks = filterd;
    }

    let mut next = "".to_string();
    if !limit.is_empty() {
        let mut size = limit.parse::<i64>().unwrap();
        size = min(size, locks.len() as i64);

        if size + 1 < locks.len() as i64 {
            locks[size as usize].id.clone_into(&mut next);
        }
        let _ = locks.split_off(size as usize);
    }

    Ok((locks, next))
}

async fn lfs_get_locks(storage: Arc<LfsStorage>, refspec: &str) -> Result<Vec<Lock>, GitLFSError> {
    let result = storage.get_lock_by_id(refspec).await.unwrap();
    match result {
        Some(val) => {
            let data = val.data;
            let locks: Vec<Lock> = serde_json::from_str(&data).unwrap();
            Ok(locks)
        }
        None => Err(GitLFSError::GeneralError("".to_string())),
    }
}

async fn lfs_add_lock(
    storage: Arc<LfsStorage>,
    repo: &str,
    locks: Vec<Lock>,
) -> Result<(), GitLFSError> {
    let result = storage.get_lock_by_id(repo).await.unwrap();

    match result {
        // Update
        Some(mut val) => {
            let d = val.data.to_owned();
            let mut locks_from_data = if !d.is_empty() {
                let locks_from_data: Vec<Lock> = serde_json::from_str(&d).unwrap();
                locks_from_data
            } else {
                vec![]
            };
            let mut locks = locks;
            locks_from_data.append(&mut locks);

            locks_from_data.sort_by(|a, b| {
                a.locked_at
                    .partial_cmp(&b.locked_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let d = serde_json::to_string(&locks_from_data).unwrap();

            d.clone_into(&mut val.data);
            let res = storage.update_lock(val).await;
            match res.is_ok() {
                true => Ok(()),
                false => Err(GitLFSError::GeneralError("".to_string())),
            }
        }
        // Insert
        None => {
            let mut locks = locks;
            locks.sort_by(|a, b| {
                a.locked_at
                    .partial_cmp(&b.locked_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let data = serde_json::to_string(&locks).unwrap();
            let lock_to = lfs_locks::Model {
                id: repo.to_owned(),
                data: data.to_owned(),
            };

            let res = storage.new_lock(lock_to).await;
            match res.is_ok() {
                true => Ok(()),
                false => Err(GitLFSError::GeneralError("".to_string())),
            }
        }
    }
}

async fn lfs_get_meta(
    storage: Arc<LfsStorage>,
    v: &RequestVars,
) -> Result<MetaObject, GitLFSError> {
    let result = storage.get_lfs_object(v.oid.clone()).await.unwrap();

    match result {
        Some(val) => Ok(MetaObject {
            oid: val.oid,
            size: val.size,
            exist: val.exist,
            splited: val.splited,
        }),
        None => Err(GitLFSError::GeneralError("".to_string())),
    }
}

async fn lfs_put_meta(
    storage: Arc<LfsStorage>,
    v: &RequestVars,
    splited: bool,
) -> Result<MetaObject, GitLFSError> {
    // Check if already exist.
    let result = storage.get_lfs_object(v.oid.clone()).await.unwrap();
    if let Some(result) = result {
        return Ok(MetaObject {
            oid: result.oid,
            size: result.size,
            exist: true,
            splited: result.splited,
        });
    }

    // Put into database if not exist.
    let meta = MetaObject {
        oid: v.oid.to_string(),
        size: v.size,
        exist: true,
        splited
    };

    let meta_to = lfs_objects::Model {
        oid: meta.oid.to_owned(),
        size: meta.size.to_owned(),
        exist: true,
        splited
    };

    let res = storage.new_lfs_object(meta_to).await;
    match res {
        Ok(_) => Ok(meta),
        Err(err) => Err(GitLFSError::GeneralError(err.to_string())),
    }
}

async fn lfs_delete_meta(storage: Arc<LfsStorage>, v: &RequestVars) -> Result<(), GitLFSError> {
    let res = storage.delete_lfs_object(v.oid.to_owned()).await;
    match res {
        Ok(_) => Ok(()),
        Err(_) => Err(GitLFSError::GeneralError("".to_string())),
    }
}

async fn delete_lock(
    storage: Arc<LfsStorage>,
    repo: &str,
    _user: Option<String>,
    id: &str,
    force: bool,
) -> Result<Lock, GitLFSError> {
    let result = storage.get_lock_by_id(repo).await.unwrap();
    match result {
        // Exist, then delete.
        Some(mut val) => {
            let d = val.data.to_owned();
            let locks_from_data = if !d.is_empty() {
                let locks_from_data: Vec<Lock> = serde_json::from_str(&d).unwrap();
                locks_from_data
            } else {
                vec![]
            };

            let mut new_locks = Vec::<Lock>::new();
            let mut lock_to_delete = Lock {
                id: "".to_owned(),
                path: "".to_owned(),
                owner: None,
                locked_at: {
                    let locked_at: DateTime<Utc> = DateTime::<Utc>::MIN_UTC;
                    locked_at.to_rfc3339()
                },
            };

            for lock in locks_from_data.iter() {
                if lock.id == *id {
                    if Option::is_some(&lock.owner) && !force {
                        return Err(GitLFSError::GeneralError("".to_string()));
                    }
                    lock.id.clone_into(&mut lock_to_delete.id);
                    lock.path.clone_into(&mut lock_to_delete.path);
                    lock_to_delete.owner.clone_from(&lock.owner);
                    lock.locked_at.clone_into(&mut lock_to_delete.locked_at);
                } else if !lock.id.is_empty() {
                    new_locks.push(Lock {
                        id: lock.id.to_owned(),
                        path: lock.path.to_owned(),
                        owner: lock.owner.clone(),
                        locked_at: lock.locked_at.to_owned(),
                    });
                }
            }
            if lock_to_delete.id.is_empty() {
                return Err(GitLFSError::GeneralError("".to_string()));
            }

            // No locks remains, delete the repo from database.
            if new_locks.is_empty() {
                storage.delete_lock_by_id(repo.to_owned()).await;
                return Ok(lock_to_delete);
            }

            // Update remaining locks.
            let data = serde_json::to_string(&new_locks).unwrap();
            data.clone_into(&mut val.data);
            let res = storage.update_lock(val).await;
            match res.is_ok() {
                true => Ok(lock_to_delete),
                false => Err(GitLFSError::GeneralError("".to_string())),
            }
        }
        // Not exist, error.
        None => Err(GitLFSError::GeneralError("".to_string())),
    }
}
