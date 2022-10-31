// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::{collections::BTreeMap, convert::Infallible};

use futures::Future;
use postgres_openssl::MakeTlsConnector;
use tempfile::NamedTempFile;
use timely::progress::Antichain;
use tokio_postgres::Config;

use mz_ore::assert_contains;

use mz_stash::{
    Append, Memory, Postgres, Sqlite, Stash, StashCollection, StashError, TableTransaction,
    Timestamp, TypedCollection,
};

#[tokio::test]
async fn test_stash_memory() -> Result<(), anyhow::Error> {
    {
        let file = NamedTempFile::new()?;
        test_stash(|| async { Memory::new(Sqlite::open(Some(file.path())).unwrap()) }).await?;
    }
    {
        let file = NamedTempFile::new()?;
        test_append(|| async { Memory::new(Sqlite::open(Some(file.path())).unwrap()) }).await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_stash_sqlite() -> Result<(), anyhow::Error> {
    {
        let file = NamedTempFile::new()?;
        test_stash(|| async { Sqlite::open(Some(file.path())).unwrap() }).await?;
    }
    {
        let file = NamedTempFile::new()?;
        test_append(|| async { Sqlite::open(Some(file.path())).unwrap() }).await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_stash_postgres() -> Result<(), anyhow::Error> {
    mz_ore::test::init_logging();

    let tls = mz_postgres_util::make_tls(&Config::new()).unwrap();

    {
        // Verify invalid URLs fail on connect.
        assert!(Postgres::new("host=invalid".into(), None, tls.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("stash error: postgres: error connecting to server"));
    }

    let connstr = match std::env::var("POSTGRES_URL") {
        Ok(s) => s,
        Err(_) => {
            println!("skipping test_stash_postgres because POSTGRES_URL is not set");
            return Ok(());
        }
    };
    async fn connect(connstr: &str, tls: MakeTlsConnector, clear: bool) -> Postgres {
        if clear {
            Postgres::clear(connstr, tls.clone()).await.unwrap();
        }
        Postgres::new(connstr.to_string(), None, tls).await.unwrap()
    }
    {
        connect(&connstr, tls.clone(), true).await;
        test_stash(|| async { connect(&connstr, tls.clone(), false).await }).await?;
    }
    {
        connect(&connstr, tls.clone(), true).await;
        test_append(|| async { connect(&connstr, tls.clone(), false).await }).await?;
    }
    // Test the fence.
    {
        let mut conn1 = connect(&connstr, tls.clone(), true).await;
        // Don't clear the stash tables.
        let mut conn2 = connect(&connstr, tls.clone(), false).await;
        assert!(match conn1.collection::<String, String>("c").await {
            Err(e) => e.is_unrecoverable(),
            _ => panic!("expected error"),
        });
        let _: StashCollection<String, String> = conn2.collection("c").await?;
    }
    // Test readonly.
    {
        let mut stash_rw = Postgres::new(connstr.to_string(), None, tls.clone())
            .await
            .unwrap();
        stash_rw.collection::<i64, i64>("c1").await.unwrap();
        let col_rw = stash_rw.collection::<i64, i64>("c1").await.unwrap();
        let mut batch = col_rw.make_batch(&mut stash_rw).await.unwrap();
        col_rw.append_to_batch(&mut batch, &1, &2, 1);
        stash_rw.append(&[batch]).await.unwrap();

        // Now make a readonly stash. We should fail to create new collections,
        // but be able to read existing collections.
        let mut stash_ro = Postgres::new_readonly(connstr.to_string(), None, tls)
            .await
            .unwrap();
        let res = stash_ro.collection::<i64, i64>("c2").await;
        assert_contains!(
            res.unwrap_err().to_string(),
            "cannot execute INSERT in a read-only transaction"
        );
        let col_ro = stash_ro.collection::<i64, i64>("c1").await.unwrap();
        assert_eq!(stash_ro.peek(col_ro).await.unwrap(), vec![(1, 2, 1)],);

        // The previous stash should still be the leader.
        assert!(stash_rw.confirm_leadership().await.is_ok());
    }

    Ok(())
}

async fn test_append<F, S, O>(f: F) -> Result<(), anyhow::Error>
where
    S: Append,
    O: Future<Output = S>,
    F: Fn() -> O,
{
    const TYPED: TypedCollection<String, String> = TypedCollection::new("typed");

    let mut stash = f().await;

    // Can't peek if since == upper.
    assert!(TYPED
        .peek_one(&mut stash)
        .await
        .unwrap_err()
        .to_string()
        .contains("since {-9223372036854775808} is not less than upper {-9223372036854775808}"));
    TYPED
        .upsert_key(&mut stash, &"k1".to_string(), |_| {
            Ok::<_, Infallible>("v1".to_string())
        })
        .await??;
    assert_eq!(
        TYPED.peek_one(&mut stash).await.unwrap(),
        BTreeMap::from([("k1".to_string(), "v1".to_string())])
    );
    TYPED
        .upsert_key(&mut stash, &"k1".to_string(), |_| {
            Ok::<_, Infallible>("v2".to_string())
        })
        .await??;
    assert_eq!(
        TYPED.peek_one(&mut stash).await.unwrap(),
        BTreeMap::from([("k1".to_string(), "v2".to_string())])
    );
    assert_eq!(
        TYPED
            .peek_key_one(&mut stash, &"k1".to_string())
            .await
            .unwrap(),
        Some("v2".to_string()),
    );
    assert_eq!(
        TYPED
            .peek_key_one(&mut stash, &"k2".to_string())
            .await
            .unwrap(),
        None
    );
    TYPED
        .upsert(
            &mut stash,
            vec![
                ("k1".to_string(), "v3".to_string()),
                ("k2".to_string(), "v4".to_string()),
            ],
        )
        .await?;
    assert_eq!(
        TYPED.peek_one(&mut stash).await.unwrap(),
        BTreeMap::from([
            ("k1".to_string(), "v3".to_string()),
            ("k2".to_string(), "v4".to_string())
        ])
    );

    // Test append across collections.
    let orders = stash.collection::<String, String>("orders").await?;
    let other = stash.collection::<String, String>("other").await?;
    // Seal so we can invalidate the upper below.
    stash.seal(other, Antichain::from_elem(1).borrow()).await?;
    let mut orders_batch = orders.make_batch(&mut stash).await?;
    orders.append_to_batch(&mut orders_batch, &"k1".to_string(), &"v1".to_string(), 1);
    let mut other_batch = other.make_batch(&mut stash).await?;
    other.append_to_batch(&mut other_batch, &"k2".to_string(), &"v2".to_string(), 1);

    // Invalidate one upper and ensure append doesn't commit partial batches.
    let other_upper = other_batch.upper;
    other_batch.upper = Antichain::from_elem(Timestamp::MIN);
    assert_eq!(
          stash
            .append(&[orders_batch.clone(), other_batch.clone()]).await
            .unwrap_err()
            .to_string(),
        "stash error: seal request {-9223372036854775808} is less than the current upper frontier {1}",
    );
    // Test batches in the other direction too.
    assert_eq!(
        stash
            .append(&[other_batch.clone(),orders_batch.clone() ]).await
            .unwrap_err()
            .to_string(),
        "stash error: seal request {-9223372036854775808} is less than the current upper frontier {1}",
    );

    // Fix the upper, append should work now.
    other_batch.upper = other_upper;
    stash.append(&[other_batch, orders_batch]).await?;
    assert_eq!(
        stash.iter(orders).await?,
        &[(("k1".into(), "v1".into()), -9223372036854775808, 1),]
    );
    assert_eq!(
        stash.iter(other).await?,
        &[(("k2".into(), "v2".into()), 1, 1),]
    );
    assert_eq!(
        stash.peek_one(orders).await?,
        BTreeMap::from([("k1".to_string(), "v1".to_string())])
    );
    assert_eq!(
        stash.peek_one(other).await?,
        BTreeMap::from([("k2".to_string(), "v2".to_string())])
    );

    // Verify the upper got bumped.
    assert_eq!(
        stash.since(orders).await?.into_option().unwrap(),
        stash.upper(orders).await?.into_option().unwrap() - 1
    );
    // Multiple empty batches should bump the upper and the since because append
    // must also compact and consolidate.
    for _ in 0..5 {
        let orders_batch = orders.make_batch(&mut stash).await?;
        stash.append(&[orders_batch]).await?;
        assert_eq!(
            stash.since(orders).await?.into_option().unwrap(),
            stash.upper(orders).await?.into_option().unwrap() - 1
        );
    }

    // Remake the stash and ensure data remains.
    let mut stash = f().await;
    assert_eq!(
        stash.peek_one(orders).await?,
        BTreeMap::from([("k1".to_string(), "v1".to_string())])
    );
    assert_eq!(
        stash.peek_one(other).await?,
        BTreeMap::from([("k2".to_string(), "v2".to_string())])
    );
    // Remake again, mutate before reading, then read.
    let mut stash = f().await;
    stash
        .update_many(orders, [(("k3".into(), "v3".into()), 1, 1)])
        .await?;
    stash.seal(orders, Antichain::from_elem(2).borrow()).await?;

    assert_eq!(
        stash.peek_one(orders).await?,
        BTreeMap::from([
            ("k1".to_string(), "v1".to_string()),
            ("k3".to_string(), "v3".to_string())
        ])
    );

    // Remake the stash, mutate, then read.
    let mut stash = f().await;
    let mut orders_batch = orders.make_batch(&mut stash).await?;
    orders.append_to_batch(&mut orders_batch, &"k4".to_string(), &"v4".to_string(), 1);
    stash.append(&[orders_batch]).await?;
    assert_eq!(
        stash.peek_one(orders).await?,
        BTreeMap::from([
            ("k1".to_string(), "v1".to_string()),
            ("k3".to_string(), "v3".to_string()),
            ("k4".to_string(), "v4".to_string())
        ])
    );

    // Remake and read again.
    let mut stash = f().await;
    assert_eq!(
        stash.peek_one(orders).await?,
        BTreeMap::from([
            ("k1".to_string(), "v1".to_string()),
            ("k3".to_string(), "v3".to_string()),
            ("k4".to_string(), "v4".to_string())
        ])
    );

    test_stash_table(&mut stash).await?;

    Ok(())
}

async fn test_stash<F, S, O>(f: F) -> Result<(), anyhow::Error>
where
    S: Stash,
    O: Future<Output = S>,
    F: Fn() -> O,
{
    let mut stash = f().await;
    // Create an arrangement, write some data into it, then read it back.
    let orders = stash.collection::<String, String>("orders").await?;
    stash
        .update(orders, ("widgets".into(), "1".into()), 1, 1)
        .await?;
    stash
        .update(orders, ("wombats".into(), "2".into()), 1, 2)
        .await?;
    // Move this before iter to better test the memory stash's iter_key.
    assert_eq!(
        stash.iter_key(orders, &"widgets".to_string()).await?,
        &[("1".into(), 1, 1)]
    );
    assert_eq!(
        stash.iter(orders).await?,
        &[
            (("widgets".into(), "1".into()), 1, 1),
            (("wombats".into(), "2".into()), 1, 2),
        ]
    );
    assert_eq!(
        stash.iter_key(orders, &"wombats".to_string()).await?,
        &[("2".into(), 1, 2)]
    );

    // Write to another arrangement and ensure the data stays separate.
    let other = stash.collection::<String, String>("other").await?;
    stash
        .update(other, ("foo".into(), "bar".into()), 1, 1)
        .await?;
    assert_eq!(
        stash.iter(other).await?,
        &[(("foo".into(), "bar".into()), 1, 1)],
    );
    assert_eq!(
        stash.iter(orders).await?,
        &[
            (("widgets".into(), "1".into()), 1, 1),
            (("wombats".into(), "2".into()), 1, 2),
        ]
    );

    // Check that consolidation happens immediately...
    stash
        .update(orders, ("wombats".into(), "2".into()), 1, -1)
        .await?;
    assert_eq!(
        stash.iter(orders).await?,
        &[
            (("widgets".into(), "1".into()), 1, 1),
            (("wombats".into(), "2".into()), 1, 1),
        ]
    );

    // ...even when it results in a entry's removal.
    stash
        .update(orders, ("wombats".into(), "2".into()), 1, -1)
        .await?;
    assert_eq!(
        stash.iter(orders).await?,
        &[(("widgets".into(), "1".into()), 1, 1),]
    );

    // Check that logical compaction applies immediately.
    stash
        .update_many(
            orders,
            [
                (("widgets".into(), "1".into()), 2, 1),
                (("widgets".into(), "1".into()), 3, 1),
                (("widgets".into(), "1".into()), 4, 1),
            ],
        )
        .await?;
    stash.seal(orders, Antichain::from_elem(3).borrow()).await?;
    // Peek should not observe widgets from timestamps 3 or 4.
    assert_eq!(stash.peek_timestamp(orders).await?, 2);
    assert_eq!(
        stash.peek(orders).await?,
        vec![("widgets".into(), "1".into(), 2)]
    );
    assert_eq!(
        stash.peek_one(orders).await.unwrap_err().to_string(),
        "stash error: unexpected peek multiplicity"
    );
    stash
        .compact(orders, Antichain::from_elem(3).borrow())
        .await?;
    assert_eq!(
        stash.iter(orders).await?,
        &[
            (("widgets".into(), "1".into()), 3, 3),
            (("widgets".into(), "1".into()), 4, 1),
        ]
    );

    // Check that physical compaction does not change the collection's contents.
    stash.consolidate(orders.id).await?;
    assert_eq!(
        stash.iter(orders).await?,
        &[
            (("widgets".into(), "1".into()), 3, 3),
            (("widgets".into(), "1".into()), 4, 1),
        ]
    );

    // Test invalid seals, compactions, and updates.
    assert_eq!(
        stash
            .seal(orders, Antichain::from_elem(2).borrow())
            .await
            .unwrap_err()
            .to_string(),
        "stash error: seal request {2} is less than the current upper frontier {3}",
    );
    assert_eq!(
        stash
            .compact(orders, Antichain::from_elem(2).borrow())
            .await
            .unwrap_err()
            .to_string(),
        "stash error: compact request {2} is less than the current since frontier {3}",
    );
    assert_eq!(
        stash
            .compact(orders, Antichain::from_elem(4).borrow())
            .await
            .unwrap_err()
            .to_string(),
        "stash error: compact request {4} is greater than the current upper frontier {3}",
    );
    assert_eq!(
        stash
            .update(orders, ("wodgets".into(), "1".into()), 2, 1)
            .await
            .unwrap_err()
            .to_string(),
        "stash error: entry time 2 is less than the current upper frontier {3}",
    );

    // Test advancing since and upper to the empty frontier.
    stash.seal(orders, Antichain::new().borrow()).await?;
    stash.compact(orders, Antichain::new().borrow()).await?;
    assert_eq!(
        match stash.iter(orders).await {
            Ok(_) => panic!("call to iter unexpectedly succeeded"),
            Err(e) => e.to_string(),
        },
        "stash error: cannot iterate collection with empty since frontier",
    );
    assert_eq!(
        match stash.iter_key(orders, &"wombats".to_string()).await {
            Ok(_) => panic!("call to iter_key unexpectedly succeeded"),
            Err(e) => e.to_string(),
        },
        "stash error: cannot iterate collection with empty since frontier",
    );
    stash.consolidate(orders.id).await?;

    // Double check that the other collection is still untouched.
    assert_eq!(
        stash.iter(other).await?,
        &[(("foo".into(), "bar".into()), 1, 1)],
    );
    assert_eq!(
        stash.since(other).await?,
        Antichain::from_elem(Timestamp::MIN)
    );
    assert_eq!(
        stash.upper(other).await?,
        Antichain::from_elem(Timestamp::MIN)
    );

    // Test peek_one.
    stash.seal(other, Antichain::from_elem(2).borrow()).await?;
    assert_eq!(
        stash.peek_one(other).await?,
        BTreeMap::from([("foo".to_string(), "bar".to_string())])
    );
    assert_eq!(
        stash.peek_key_one(other, &"foo".to_string()).await?,
        Some("bar".to_string())
    );

    Ok(())
}

async fn test_stash_table(stash: &mut impl Append) -> Result<(), anyhow::Error> {
    const TABLE: TypedCollection<Vec<u8>, String> = TypedCollection::new("table");
    fn uniqueness_violation(a: &String, b: &String) -> bool {
        a == b
    }
    let collection = TABLE.get(stash).await?;

    async fn commit(
        stash: &mut impl Append,
        collection: StashCollection<Vec<u8>, String>,
        pending: Vec<(Vec<u8>, String, i64)>,
    ) -> Result<(), StashError> {
        let mut batch = collection.make_batch(stash).await?;
        for (k, v, diff) in pending {
            collection.append_to_batch(&mut batch, &k, &v, diff);
        }
        stash.append(&[batch]).await?;
        Ok(())
    }

    TABLE
        .upsert_key(stash, &1i64.to_le_bytes().to_vec(), |_| {
            Ok::<_, Infallible>("v1".to_string())
        })
        .await??;
    TABLE
        .upsert(stash, vec![(2i64.to_le_bytes().to_vec(), "v2".to_string())])
        .await?;
    let mut table = TableTransaction::new(TABLE.peek_one(stash).await?, uniqueness_violation);
    assert_eq!(
        table.items(),
        BTreeMap::from([
            (1i64.to_le_bytes().to_vec(), "v1".to_string()),
            (2i64.to_le_bytes().to_vec(), "v2".to_string())
        ])
    );
    assert_eq!(table.delete(|_k, _v| false).len(), 0);
    assert_eq!(table.delete(|_k, v| v == "v2").len(), 1);
    assert_eq!(
        table.items(),
        BTreeMap::from([(1i64.to_le_bytes().to_vec(), "v1".to_string())])
    );
    assert_eq!(table.update(|_k, _v| Some("v3".to_string()))?, 1);

    // Uniqueness violation.
    table
        .insert(3i64.to_le_bytes().to_vec(), "v3".to_string())
        .unwrap_err();

    table
        .insert(3i64.to_le_bytes().to_vec(), "v4".to_string())
        .unwrap();
    assert_eq!(
        table.items(),
        BTreeMap::from([
            (1i64.to_le_bytes().to_vec(), "v3".to_string()),
            (3i64.to_le_bytes().to_vec(), "v4".to_string()),
        ])
    );
    assert_eq!(
        table
            .update(|_k, _v| Some("v1".to_string()))
            .unwrap_err()
            .to_string(),
        "stash error: uniqueness violation"
    );
    let pending = table.pending();
    assert_eq!(
        pending,
        vec![
            (1i64.to_le_bytes().to_vec(), "v1".to_string(), -1),
            (1i64.to_le_bytes().to_vec(), "v3".to_string(), 1),
            (2i64.to_le_bytes().to_vec(), "v2".to_string(), -1),
            (3i64.to_le_bytes().to_vec(), "v4".to_string(), 1),
        ]
    );
    commit(stash, collection, pending).await?;
    let items = TABLE.peek_one(stash).await?;
    assert_eq!(
        items,
        BTreeMap::from([
            (1i64.to_le_bytes().to_vec(), "v3".to_string()),
            (3i64.to_le_bytes().to_vec(), "v4".to_string())
        ])
    );

    let mut table = TableTransaction::new(items, uniqueness_violation);
    // Deleting then creating an item that has a uniqueness violation should work.
    assert_eq!(table.delete(|k, _v| k == &1i64.to_le_bytes()).len(), 1);
    table
        .insert(1i64.to_le_bytes().to_vec(), "v3".to_string())
        .unwrap();
    // Uniqueness violation in value.
    table
        .insert(5i64.to_le_bytes().to_vec(), "v3".to_string())
        .unwrap_err();
    // Key already exists, expect error.
    table
        .insert(1i64.to_le_bytes().to_vec(), "v5".to_string())
        .unwrap_err();
    assert_eq!(table.delete(|k, _v| k == &1i64.to_le_bytes()).len(), 1);
    // Both the inserts work now because the key and uniqueness violation are gone.
    table
        .insert(5i64.to_le_bytes().to_vec(), "v3".to_string())
        .unwrap();
    table
        .insert(1i64.to_le_bytes().to_vec(), "v5".to_string())
        .unwrap();
    let pending = table.pending();
    assert_eq!(
        pending,
        vec![
            (1i64.to_le_bytes().to_vec(), "v3".to_string(), -1),
            (1i64.to_le_bytes().to_vec(), "v5".to_string(), 1),
            (5i64.to_le_bytes().to_vec(), "v3".to_string(), 1),
        ]
    );
    commit(stash, collection, pending).await?;
    let items = TABLE.peek_one(stash).await?;
    assert_eq!(
        items,
        BTreeMap::from([
            (1i64.to_le_bytes().to_vec(), "v5".to_string()),
            (3i64.to_le_bytes().to_vec(), "v4".to_string()),
            (5i64.to_le_bytes().to_vec(), "v3".to_string()),
        ])
    );

    let mut table = TableTransaction::new(items, uniqueness_violation);
    assert_eq!(table.delete(|_k, _v| true).len(), 3);
    table
        .insert(1i64.to_le_bytes().to_vec(), "v1".to_string())
        .unwrap();

    commit(stash, collection, table.pending()).await?;
    let items = TABLE.peek_one(stash).await?;
    assert_eq!(
        items,
        BTreeMap::from([(1i64.to_le_bytes().to_vec(), "v1".to_string()),])
    );

    let mut table = TableTransaction::new(items, uniqueness_violation);
    assert_eq!(table.delete(|_k, _v| true).len(), 1);
    table
        .insert(1i64.to_le_bytes().to_vec(), "v2".to_string())
        .unwrap();
    commit(stash, collection, table.pending()).await?;
    let items = TABLE.peek_one(stash).await?;
    assert_eq!(
        items,
        BTreeMap::from([(1i64.to_le_bytes().to_vec(), "v2".to_string()),])
    );

    // Verify we don't try to delete v3 or v4 during commit.
    let mut table = TableTransaction::new(items, uniqueness_violation);
    assert_eq!(table.delete(|_k, _v| true).len(), 1);
    table
        .insert(1i64.to_le_bytes().to_vec(), "v3".to_string())
        .unwrap();
    table
        .insert(1i64.to_le_bytes().to_vec(), "v4".to_string())
        .unwrap_err();
    assert_eq!(table.delete(|_k, _v| true).len(), 1);
    table
        .insert(1i64.to_le_bytes().to_vec(), "v5".to_string())
        .unwrap();
    commit(stash, collection, table.pending()).await?;
    let items = stash.peek(collection).await?;
    assert_eq!(
        items,
        vec![(1i64.to_le_bytes().to_vec(), "v5".to_string(), 1)]
    );

    Ok(())
}

#[test]
fn test_table() {
    fn uniqueness_violation(a: &String, b: &String) -> bool {
        a == b
    }
    let mut table = TableTransaction::new(
        BTreeMap::from([(1i64.to_le_bytes().to_vec(), "a".to_string())]),
        uniqueness_violation,
    );

    table
        .insert(2i64.to_le_bytes().to_vec(), "b".to_string())
        .unwrap();
    table
        .insert(3i64.to_le_bytes().to_vec(), "c".to_string())
        .unwrap();
}
