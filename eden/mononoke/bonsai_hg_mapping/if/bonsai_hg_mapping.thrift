/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

include "eden/mononoke/mercurial/types/if/mercurial_thrift.thrift"
include "eden/mononoke/mononoke_types/if/mononoke_types_thrift.thrift"

# Memcache constants. Should be change when we want to invalidate memcache
# entries
const i32 MC_CODEVER = 0;
const i32 MC_SITEVER = 3;

typedef i32 RepoId (rust.newtype)

struct BonsaiHgMappingCacheEntry {
  1: required RepoId repo_id;
  2: required mononoke_types_thrift.ChangesetId bcs_id;
  3: required mercurial_thrift.HgNodeHash hg_cs_id;
} (rust.exhaustive)
