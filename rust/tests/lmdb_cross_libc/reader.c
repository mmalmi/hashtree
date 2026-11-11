/* Second process for the cross-libc lock test: opens the environment
 * read-only and begins one read transaction, the way an external reader of a
 * live store does. Exit 0 on success, 3 with the LMDB error otherwise. */
#include <stdio.h>
#include <stdlib.h>
#include "lmdb.h"

#define CHECK(call) do { \
	int rc_ = (call); \
	if (rc_) { \
		fprintf(stderr, "reader: %s: %s\n", #call, mdb_strerror(rc_)); \
		exit(3); \
	} \
} while (0)

int main(int argc, char **argv)
{
	MDB_env *env;
	MDB_txn *txn;

	if (argc != 2)
		return 2;
	CHECK(mdb_env_create(&env));
	CHECK(mdb_env_set_maxdbs(env, 32));
	CHECK(mdb_env_open(env, argv[1], MDB_RDONLY | MDB_NOTLS | MDB_NORDAHEAD, 0600));
	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	mdb_txn_abort(txn);
	mdb_env_close(env);
	puts("ok");
	return 0;
}
