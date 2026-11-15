use rbitcoin_primitives::Fk;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::scripthash_pages::*;
use rbitcoin_store::scripthash_slabs::encode_fk_delta_stream_into;

fn raw(fks: &[Fk]) -> Vec<u64> {
    fks.iter().map(|fk| fk.0).collect()
}

#[test]
fn sh_page_pack_matches_encoded_stream() {
    let fks: Vec<u64> = (1..=80).collect();
    let ents: Vec<_> = fks.iter().copied().map(Fk).collect();
    let raw: Vec<u64> = ents.iter().map(|fk| fk.0).collect();
    let mut stream = vec![0u8; raw.len().saturating_mul(10).max(1)];
    let sn = encode_fk_delta_stream_into(&mut stream, &raw).unwrap();
    stream.truncate(sn);
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_pack_fks(&mut page, &fks, 8192).unwrap();
    assert_eq!(sh_page_n_fks(&page).unwrap() as usize, fks.len());
    assert_eq!(sh_page_next(&page).unwrap(), 8192);
    assert_eq!(
        &page[SH_PAGE_OFF_FKS..SH_PAGE_OFF_FKS + stream.len()],
        stream.as_slice()
    );
    let wrapped = sh_page_entries(&page).unwrap();
    let mut into = Vec::new();
    sh_page_entries_into(&page, &mut into).unwrap();
    assert_eq!(into, wrapped);
    assert_eq!(wrapped.iter().map(|fk| fk.0).collect::<Vec<_>>(), fks);
    let (next, slice_ents) = sh_page_decode_slice(&page).unwrap();
    let mut slice_into = Vec::new();
    let next_into = sh_page_decode_slice_into(&page, &mut slice_into).unwrap();
    assert_eq!(next_into, next);
    assert_eq!(slice_into, slice_ents);
    assert_eq!(slice_ents, wrapped);
    let mut over = [0u8; SH_PAGE_SIZE];
    let too_many: Vec<u64> = (1..=SH_PAGE_STREAM_MAX as u64 + 2).collect();
    match sh_page_pack_fks(&mut over, &too_many, 0) {
        Err(StoreError::Corrupt(m)) => {
            assert!(
                m.contains("entries exceed page capacity"),
                "capacity error must stay the same string: {m}"
            );
        }
        other => panic!("expected capacity Corrupt, got {other:?}"),
    }
}

#[test]
fn sh_page_delta_packs_600_sequential_in_one_page() {
    let ents: Vec<_> = (1u64..=600).map(Fk).collect();
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_pack_fks(&mut page, &raw(&ents), 0).unwrap();
    assert_eq!(sh_page_n_fks(&page).unwrap(), 600);
    assert_eq!(sh_page_last_fk(&page).unwrap(), Some(Fk(600)));
    assert_eq!(sh_page_entries(&page).unwrap(), ents);
    assert_eq!(page[SH_PAGE_OFF_VER], SH_PAGE_DELTA_VER);
}

#[test]
fn sh_page_last_fk_matches_entries_last() {
    let mut empty = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut empty);
    assert_eq!(sh_page_last_fk(&empty).unwrap(), None);
    assert!(sh_page_entries(&empty).unwrap().is_empty());

    let one = [Fk(9)];
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_pack_fks(&mut page, &raw(&one), 0).unwrap();
    assert_eq!(
        sh_page_last_fk(&page).unwrap(),
        sh_page_entries(&page).unwrap().last().copied()
    );

    let ents: Vec<_> = (1u64..=600).map(Fk).collect();
    let mut big = [0u8; SH_PAGE_SIZE];
    sh_page_pack_fks(&mut big, &raw(&ents), 0).unwrap();
    assert_eq!(
        sh_page_last_fk(&big).unwrap(),
        sh_page_entries(&big).unwrap().last().copied()
    );
    assert_eq!(sh_page_last_fk(&big).unwrap(), Some(Fk(600)));
}

#[test]
fn sh_page_delta_append_opens_second_page_when_stream_full() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    let mut n = 0u64;
    loop {
        n += 1;
        if !sh_page_try_append(&mut page, Fk(n)).unwrap() {
            break;
        }
    }
    assert!(
        n > SH_PAGE_FK_CAP as u64,
        "delta must beat 510 raw slots, n={n}"
    );
    assert!(sh_page_n_fks(&page).unwrap() as u64 >= SH_PAGE_FK_CAP as u64);
    assert_eq!(sh_page_last_fk(&page).unwrap(), Some(Fk(n - 1)));
    assert!(!sh_page_try_append(&mut page, Fk(n)).unwrap());
}

#[test]
fn sh_page_delta_refuses_raw_u64_leftover() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    page[SH_PAGE_OFF_VER] = 0;
    page[SH_PAGE_OFF_N_FKS..SH_PAGE_OFF_N_FKS + 2].copy_from_slice(&1u16.to_le_bytes());
    page[SH_PAGE_OFF_FKS..SH_PAGE_OFF_FKS + 8].copy_from_slice(&1u64.to_le_bytes());
    match sh_page_entries(&page) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("rematerialize") || m.contains("raw"), "{m}");
        }
        other => panic!("expected leftover Corrupt, got {other:?}"),
    }
}

#[test]
fn layout_constants_pin_32b_slot_and_4k_page() {
    // Full OA slot stays 32 B (documented contract; head module owns key len).
    assert_eq!(rbitcoin_store::scripthash_layout::SH_HEAD_SLOT_SIZE, 24);
    assert_eq!(rbitcoin_store::scripthash_layout::SH_HEAD_VALUE_LEN, 8);
    assert_eq!(SH_PAGE_SIZE, 4096);
    assert_eq!(SH_PAGE_HEADER_LEN, 8);
    assert_eq!(SH_PAGE_FK_CAP, 511);
    assert_eq!(SH_FLAG_BIT, 1u64 << 63);
}

#[test]
fn page_last_and_next_roundtrip() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    assert!(sh_page_is_last(&page).unwrap());
    assert_eq!(sh_page_next(&page).unwrap(), 0);
    assert_eq!(sh_page_first_off(&page).unwrap(), 0);
    sh_page_set_next(&mut page, 8192).unwrap();
    assert!(!sh_page_is_last(&page).unwrap());
    assert_eq!(sh_page_next(&page).unwrap(), 8192);
    assert!(sh_page_first_off(&page).is_err());
    sh_page_set_last(&mut page, 4096).unwrap();
    assert!(sh_page_is_last(&page).unwrap());
    assert_eq!(sh_page_next(&page).unwrap(), 0);
    assert_eq!(sh_page_first_off(&page).unwrap(), 4096);
    assert!(sh_page_set_next(&mut page, 100).is_err());
}

#[test]
fn page_last_header_holds_first_off() {
    let mut first = [0u8; SH_PAGE_SIZE];
    let mut last = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut first);
    sh_page_init_empty(&mut last);
    sh_page_set_next(&mut first, 8192).unwrap();
    sh_page_set_last(&mut last, 4096).unwrap();
    assert_eq!(sh_page_next(&first).unwrap(), 8192);
    assert_eq!(sh_page_next(&last).unwrap(), 0);
    assert_eq!(sh_page_first_off(&last).unwrap(), 4096);
    assert_eq!(SH_PAGE_HEADER_LEN, 8);
}

#[test]
fn sh_page_ver2_extent_roundtrip() {
    let ents: Vec<_> = (1u64..=3).map(Fk).collect();
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_pack_extent_last_fks(&mut page, &raw(&ents), 4096, 2, 0).unwrap();
    assert_eq!(page[SH_PAGE_OFF_VER], SH_PAGE_EXTENT_VER);
    assert_eq!(sh_page_extent(&page).unwrap(), Some((4096, 2)));
    assert_eq!(sh_page_next(&page).unwrap(), 0);
    assert_eq!(sh_page_first_off(&page).unwrap(), 4096);
    assert_eq!(sh_page_entries(&page).unwrap(), ents);
    let mut tailed = [0u8; SH_PAGE_SIZE];
    sh_page_pack_extent_last_fks(&mut tailed, &raw(&ents), 4096, 2, 12288).unwrap();
    assert_eq!(sh_page_next(&tailed).unwrap(), 12288);
    assert_eq!(sh_page_extent(&tailed).unwrap(), Some((4096, 2)));
    assert!(!sh_page_is_last(&tailed).unwrap());
}

/// Sequential 1-byte deltas of length `STREAM_MAX` fit `ver=1` but not `ver=2`
/// (16 B extra extent header). Chunking must leave the last page ≤ extent cap
/// so cold materialize `pack_extent_last` cannot overflow.
#[test]
fn chunk_ranges_last_page_fits_extent_header() {
    let n = SH_PAGE_EXTENT_STREAM_MAX + 8;
    assert!(n <= SH_PAGE_STREAM_MAX);
    let fks: Vec<Fk> = (1..=n as u64).map(Fk).collect();
    assert!(n > SH_PAGE_EXTENT_STREAM_MAX);
    let chunks = sh_page_chunk_ranges(&fks).unwrap();
    assert!(
        chunks.len() >= 2,
        "last page must split so ver=2 header fits, got {chunks:?}"
    );
    let n_pages = chunks.len() as u32;
    let mut got = Vec::new();
    for (pi, &(start, end)) in chunks.iter().enumerate() {
        let mut page = [0u8; SH_PAGE_SIZE];
        if pi + 1 == chunks.len() {
            sh_page_pack_extent_last_fks(&mut page, &raw(&fks[start..end]), 4096, n_pages, 0)
                .expect("last chunk must pack as ver=2");
            assert_eq!(sh_page_extent(&page).unwrap(), Some((4096, n_pages)));
        } else {
            let next = 4096 + ((pi as u64) + 1) * (SH_PAGE_SIZE as u64);
            sh_page_pack_fks(&mut page, &raw(&fks[start..end]), next).unwrap();
        }
        got.extend(sh_page_entries(&page).unwrap());
    }
    assert_eq!(got, fks);
}

#[test]
fn sh_page_ver2_rejects_bad_extent() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    assert!(sh_page_set_extent(&mut page, 4096, 0).is_err());
    assert!(sh_page_set_extent(&mut page, 4097, 1).is_err());
    assert!(sh_page_set_extent(&mut page, 0, 1).is_err());
    sh_page_set_extent(&mut page, 8192, 3).unwrap();
    page[SH_PAGE_OFF_EXTENT_N..SH_PAGE_OFF_EXTENT_N + 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(sh_page_extent(&page).is_err());
    sh_page_set_extent(&mut page, 8192, 3).unwrap();
    page[SH_PAGE_OFF_EXTENT_BASE..SH_PAGE_OFF_EXTENT_BASE + 8]
        .copy_from_slice(&100u64.to_le_bytes());
    assert!(sh_page_extent(&page).is_err());
    let mut v1 = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut v1);
    assert_eq!(sh_page_extent(&v1).unwrap(), None);
}

#[test]
fn page_append_fill_and_next_link() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    assert_eq!(sh_page_n_fks(&page).unwrap(), 0);
    assert_eq!(sh_page_next(&page).unwrap(), 0);
    assert!(sh_page_try_append(&mut page, Fk(1)).unwrap());
    assert!(sh_page_try_append(&mut page, Fk(2)).unwrap());
    assert_eq!(sh_page_entries(&page).unwrap(), vec![Fk(1), Fk(2)]);
    sh_page_set_next(&mut page, 8192).unwrap();
    assert_eq!(sh_page_next(&page).unwrap(), 8192);

    let mut full = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut full);
    let mut i = 1u64;
    while sh_page_try_append(&mut full, Fk(i)).unwrap() {
        i += 1;
    }
    assert!(i > SH_PAGE_FK_CAP as u64);
    assert!(!sh_page_try_append(&mut full, Fk(i)).unwrap());
    assert_eq!(sh_page_entries(&full).unwrap().len() as u64, i - 1);
}

#[test]
fn page_rejects_flagged_fk_and_null() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    assert!(sh_page_try_append(&mut page, Fk::NULL).is_err());
    assert!(sh_page_try_append(&mut page, Fk(SH_FLAG_BIT | 1)).is_err());
}

#[test]
fn page_count_for_entries_and_pack_sets_next_before_write() {
    let n = SH_PAGE_STREAM_MAX + 200;
    let fks: Vec<Fk> = (1..=n as u64).map(Fk).collect();
    let chunks = sh_page_chunk_ranges(&fks).unwrap();
    assert!(chunks.len() >= 2, "stream overflow must split pages");
    let base = 4096u64;
    let mut pages = Vec::new();
    for (pi, &(start, end)) in chunks.iter().enumerate() {
        let off = base + (pi as u64) * (SH_PAGE_SIZE as u64);
        let next = if pi + 1 < chunks.len() {
            off + SH_PAGE_SIZE as u64
        } else {
            0
        };
        let mut page = [0u8; SH_PAGE_SIZE];
        sh_page_pack_fks(&mut page, &raw(&fks[start..end]), next).unwrap();
        assert_eq!(sh_page_next(&page).unwrap(), next);
        assert_eq!(sh_page_n_fks(&page).unwrap() as usize, end - start);
        pages.push(page);
    }
    assert_eq!(sh_page_next(&pages[0]).unwrap(), base + SH_PAGE_SIZE as u64);
    assert_eq!(sh_page_next(pages.last().unwrap()).unwrap(), 0);
    let mut got = Vec::new();
    for p in &pages {
        got.extend(sh_page_entries(p).unwrap());
    }
    assert_eq!(got, fks);
    // One page cannot hold STREAM_MAX+1 one-byte deltas (first fk + N gaps).
    let too_many: Vec<u64> = (1..=SH_PAGE_STREAM_MAX as u64 + 2).collect();
    let mut page = [0u8; SH_PAGE_SIZE];
    assert!(sh_page_pack_fks(&mut page, &too_many, 0).is_err());
}

#[test]
fn page_fks_must_be_strictly_increasing() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    assert!(sh_page_try_append(&mut page, Fk(10)).unwrap());
    assert!(sh_page_try_append(&mut page, Fk(20)).unwrap());
    // Equal / lower rejected at append (encode bug), not silent.
    assert!(sh_page_try_append(&mut page, Fk(20)).is_err());
    assert!(sh_page_try_append(&mut page, Fk(5)).is_err());
    assert_eq!(sh_page_last_fk(&page).unwrap(), Some(Fk(20)));

    let unsorted = [3u64, 1, 2];
    assert!(sh_page_pack_fks(&mut page, &unsorted, 0).is_err());

    // Decode refuses durable unsorted bytes (plant equal consecutive).
    sh_page_init_empty(&mut page);
    sh_page_try_append(&mut page, Fk(1)).unwrap();
    sh_page_try_append(&mut page, Fk(2)).unwrap();
    // Zero the second-stream uleb (delta) without going through append.
    page[SH_PAGE_OFF_FKS + 1] = 0;
    assert!(sh_page_entries(&page).is_err());
}

#[test]
fn page_slice_must_be_exactly_4k() {
    let short = [0u8; 100];
    assert!(sh_page_as_array(&short).is_err());
    assert!(sh_page_decode_slice(&short).is_err());
    let mut long = vec![0u8; SH_PAGE_SIZE + 1];
    assert!(sh_page_as_array(&long).is_err());
    assert!(sh_page_as_array_mut(&mut long[..SH_PAGE_SIZE - 1]).is_err());

    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    sh_page_try_append(&mut page, Fk(7)).unwrap();
    let (next, ents) = sh_page_decode_slice(&page).unwrap();
    assert_eq!(next, 0);
    assert_eq!(ents, vec![Fk(7)]);
}

#[test]
fn page_corrupt_n_fks_over_capacity() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    page[SH_PAGE_OFF_N_FKS..SH_PAGE_OFF_N_FKS + 2]
        .copy_from_slice(&((SH_PAGE_STREAM_MAX + 1) as u16).to_le_bytes());
    assert!(sh_page_n_fks(&page).is_err());
    assert!(sh_page_entries(&page).is_err());
}

#[test]
fn page_entries_reject_null_and_flagged_fk_bytes() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    page[SH_PAGE_OFF_N_FKS..SH_PAGE_OFF_N_FKS + 2].copy_from_slice(&1u16.to_le_bytes());
    page[SH_PAGE_OFF_FKS] = 0;
    assert!(sh_page_entries(&page).is_err());
    let mut stream = Vec::new();
    rbitcoin_primitives::write_uleb128(&mut stream, SH_FLAG_BIT | 9);
    page[SH_PAGE_OFF_FKS..SH_PAGE_OFF_FKS + stream.len()].copy_from_slice(&stream);
    assert!(sh_page_entries(&page).is_err());
}

#[test]
fn page_last_fk_rejects_null_and_flagged() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    assert_eq!(sh_page_last_fk(&page).unwrap(), None);
    page[SH_PAGE_OFF_N_FKS..SH_PAGE_OFF_N_FKS + 2].copy_from_slice(&1u16.to_le_bytes());
    page[SH_PAGE_OFF_FKS] = 0;
    assert!(sh_page_last_fk(&page).is_err());
    let mut flagged = Vec::new();
    rbitcoin_primitives::write_uleb128(&mut flagged, SH_FLAG_BIT | 42);
    page[SH_PAGE_OFF_FKS..SH_PAGE_OFF_FKS + flagged.len()].copy_from_slice(&flagged);
    assert!(sh_page_last_fk(&page).is_err());
    sh_page_init_empty(&mut page);
    assert!(sh_page_try_append(&mut page, Fk(42)).unwrap());
    assert_eq!(sh_page_last_fk(&page).unwrap(), Some(Fk(42)));
}

#[test]
fn sh_page_extent_and_count_roundtrip() {
    let mut page = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut page);
    sh_page_set_extent(&mut page, 8192, 3).unwrap();
    sh_page_set_extent_creates(&mut page, 11);
    sh_page_set_n_fks(&mut page, 6);
    assert_eq!(sh_page_extent(&page).unwrap(), Some((8192, 3)));
    assert_eq!(sh_page_extent_creates(&page), 11);
    assert_eq!(sh_page_n_fks(&page).unwrap(), 6);
    let mut plain = [0u8; SH_PAGE_SIZE];
    sh_page_init_empty(&mut plain);
    assert_eq!(sh_page_extent(&plain).unwrap(), None);
    assert_eq!(sh_page_extent_creates(&plain), 0);
}
