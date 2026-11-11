/* Store holder for the cross-libc lock test: opens an LMDB environment with
 * locking, commits once, then commits again for every "write" line read on
 * stdin. A "hold" line makes it keep the writer lock until it is killed.
 * It stands in for a daemon that keeps a store open. */
#include <stdio.h>
#include <stdlib.h>
#include "lmdb.h"

#define CHECK(call) do { \
	int rc_ = (call); \
	if (rc_) { \
		fprintf(stderr, "holder: %s: %s\n", #call, mdb_strerror(rc_)); \
		exit(3); \
	} \
} while (0)

static void commit_once(MDB_env *env, int round)
{
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val key = { sizeof(round), &round };
	MDB_val value = { 5, "value" };

	CHECK(mdb_txn_begin(env, NULL, 0, &txn));
	CHECK(mdb_dbi_open(txn, NULL, 0, &dbi));
	CHECK(mdb_put(txn, dbi, &key, &value, 0));
	CHECK(mdb_txn_commit(txn));
}

int main(int argc, char **argv)
{
	MDB_env *env;
	char line[16];
	int round = 0;

	if (argc != 2)
		return 2;
	CHECK(mdb_env_create(&env));
	CHECK(mdb_env_set_maxdbs(env, 32));
	CHECK(mdb_env_open(env, argv[1], 0, 0600));
	commit_once(env, round++);
	puts("ready");
	fflush(stdout);
	while (fgets(line, sizeof(line), stdin)) {
		if (line[0] == 'h') {
			/* Take the writer lock and keep it until killed. */
			MDB_txn *txn;
			CHECK(mdb_txn_begin(env, NULL, 0, &txn));
			puts("holding");
			fflush(stdout);
			continue;
		}
		commit_once(env, round++);
		puts("wrote");
		fflush(stdout);
	}
	mdb_env_close(env);
	return 0;
}
