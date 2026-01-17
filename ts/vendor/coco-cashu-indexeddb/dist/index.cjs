//#region rolldown:runtime
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __copyProps = (to, from, except, desc) => {
	if (from && typeof from === "object" || typeof from === "function") for (var keys = __getOwnPropNames(from), i = 0, n = keys.length, key; i < n; i++) {
		key = keys[i];
		if (!__hasOwnProp.call(to, key) && key !== except) __defProp(to, key, {
			get: ((k) => from[k]).bind(null, key),
			enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable
		});
	}
	return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", {
	value: mod,
	enumerable: true
}) : target, mod));

//#endregion
let dexie = require("dexie");
dexie = __toESM(dexie);
let coco_cashu_core = require("coco-cashu-core");
coco_cashu_core = __toESM(coco_cashu_core);

//#region src/lib/db.ts
/**
* Wrapper around Dexie providing transaction management for IndexedDB.
*
* Transaction behavior:
* - Nested transactions within the same Dexie transaction context are reused
* - Concurrent transactions are queued and executed serially
* - Dexie handles automatic commit/rollback based on promise resolution/rejection
*/
var IdbDb = class extends dexie.default {
	/** Promise chain used to serialize concurrent transactions */
	transactionQueue = Promise.resolve();
	/** Currently active Dexie transaction (null if no transaction) */
	activeTransaction = null;
	constructor(options = {}) {
		super(options.name ?? "coco_cashu");
	}
	/**
	* Execute a function within a database transaction.
	*
	* Transaction Semantics:
	*
	* 1. NESTED TRANSACTIONS (same Dexie context):
	*    When runTransaction() is called from within an active transaction,
	*    Dexie.currentTransaction will be set. The inner call reuses this transaction.
	*    No new transaction is created.
	*
	* 2. CONCURRENT TRANSACTIONS (different contexts):
	*    When runTransaction() is called while another transaction is active but from
	*    a different context, the new transaction waits in a queue. This prevents
	*    conflicts and ensures serialization of operations.
	*
	* 3. ERROR HANDLING:
	*    Dexie automatically rolls back the transaction if the promise is rejected.
	*    The transaction queue is properly released even on error, allowing subsequent
	*    transactions to proceed.
	*
	* @param mode - Transaction mode: 'r' (readonly) or 'rw' (readwrite)
	* @param stores - Array of store names to include in the transaction
	* @param fn - Function to execute within the transaction, receives a Dexie transaction
	* @returns Promise that resolves with the return value of fn
	* @throws Re-throws any error from fn after Dexie rolls back the transaction
	*/
	async runTransaction(mode, stores, fn) {
		const currentTx = dexie.default.currentTransaction;
		if (currentTx && currentTx === this.activeTransaction && currentTx.active) return fn(currentTx);
		const previousTransaction = this.transactionQueue;
		let resolver;
		this.transactionQueue = new Promise((resolve) => {
			resolver = resolve;
		});
		try {
			await previousTransaction;
			return await this.transaction(mode, stores, async (tx) => {
				const previousActive = this.activeTransaction;
				this.activeTransaction = tx;
				try {
					return await fn(tx);
				} finally {
					this.activeTransaction = previousActive;
				}
			});
		} finally {
			resolver();
		}
	}
	get currentTransaction() {
		return dexie.default.currentTransaction ?? this.activeTransaction;
	}
};

//#endregion
//#region src/lib/schema.ts
async function ensureSchema(db) {
	db.version(1).stores({
		coco_cashu_mints: "&mintUrl, name, updatedAt",
		coco_cashu_keysets: "&[mintUrl+id], mintUrl, id, updatedAt",
		coco_cashu_counters: "&[mintUrl+keysetId]",
		coco_cashu_proofs: "&[mintUrl+secret], [mintUrl+state], [mintUrl+id+state], state, mintUrl, id",
		coco_cashu_mint_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_melt_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_history: "++id, mintUrl, type, createdAt, [mintUrl+quoteId+type]"
	});
	db.version(2).stores({
		coco_cashu_mints: "&mintUrl, name, updatedAt, trusted",
		coco_cashu_keysets: "&[mintUrl+id], mintUrl, id, updatedAt",
		coco_cashu_counters: "&[mintUrl+keysetId]",
		coco_cashu_proofs: "&[mintUrl+secret], [mintUrl+state], [mintUrl+id+state], state, mintUrl, id",
		coco_cashu_mint_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_melt_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_history: "++id, mintUrl, type, createdAt, [mintUrl+quoteId+type]"
	}).upgrade(async (tx) => {
		const mints = await tx.table("coco_cashu_mints").toArray();
		for (const mint of mints) await tx.table("coco_cashu_mints").update(mint.mintUrl, { trusted: true });
	});
	db.version(3).stores({
		coco_cashu_mints: "&mintUrl, name, updatedAt, trusted",
		coco_cashu_keysets: "&[mintUrl+id], mintUrl, id, updatedAt, unit",
		coco_cashu_counters: "&[mintUrl+keysetId]",
		coco_cashu_proofs: "&[mintUrl+secret], [mintUrl+state], [mintUrl+id+state], state, mintUrl, id",
		coco_cashu_mint_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_melt_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_history: "++id, mintUrl, type, createdAt, [mintUrl+quoteId+type]"
	});
	db.version(4).stores({
		coco_cashu_mints: "&mintUrl, name, updatedAt, trusted",
		coco_cashu_keysets: "&[mintUrl+id], mintUrl, id, updatedAt, unit",
		coco_cashu_counters: "&[mintUrl+keysetId]",
		coco_cashu_proofs: "&[mintUrl+secret], [mintUrl+state], [mintUrl+id+state], state, mintUrl, id",
		coco_cashu_mint_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_melt_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_history: "++id, mintUrl, type, createdAt, [mintUrl+quoteId+type]",
		coco_cashu_keypairs: "&publicKey, createdAt, derivationIndex"
	});
	db.version(5).stores({
		coco_cashu_mints: "&mintUrl, name, updatedAt, trusted",
		coco_cashu_keysets: "&[mintUrl+id], mintUrl, id, updatedAt, unit",
		coco_cashu_counters: "&[mintUrl+keysetId]",
		coco_cashu_proofs: "&[mintUrl+secret], [mintUrl+state], [mintUrl+id+state], state, mintUrl, id",
		coco_cashu_mint_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_melt_quotes: "&[mintUrl+quote], state, mintUrl",
		coco_cashu_history: "++id, mintUrl, type, createdAt, [mintUrl+quoteId+type]",
		coco_cashu_keypairs: "&publicKey, createdAt, derivationIndex"
	}).upgrade(async (tx) => {
		const mints = await tx.table("coco_cashu_mints").toArray();
		const urlMapping = /* @__PURE__ */ new Map();
		for (const mint of mints) {
			const normalized = (0, coco_cashu_core.normalizeMintUrl)(mint.mintUrl);
			urlMapping.set(mint.mintUrl, normalized);
		}
		const normalizedToOriginal = /* @__PURE__ */ new Map();
		for (const [original, normalized] of urlMapping) {
			const existing = normalizedToOriginal.get(normalized);
			if (existing && existing !== original) throw new Error(`Mint URL normalization conflict: "${existing}" and "${original}" both normalize to "${normalized}". Please manually resolve this conflict before running the migration.`);
			normalizedToOriginal.set(normalized, original);
		}
		for (const [original, normalized] of urlMapping) {
			if (original === normalized) continue;
			const mint = await tx.table("coco_cashu_mints").get(original);
			if (mint) {
				await tx.table("coco_cashu_mints").delete(original);
				await tx.table("coco_cashu_mints").add({
					...mint,
					mintUrl: normalized
				});
			}
			const keysets = await tx.table("coco_cashu_keysets").where("mintUrl").equals(original).toArray();
			for (const keyset of keysets) {
				await tx.table("coco_cashu_keysets").delete([original, keyset.id]);
				await tx.table("coco_cashu_keysets").add({
					...keyset,
					mintUrl: normalized
				});
			}
			const counters = await tx.table("coco_cashu_counters").where("[mintUrl+keysetId]").between([original, ""], [original, "￿"]).toArray();
			for (const counter of counters) {
				await tx.table("coco_cashu_counters").delete([original, counter.keysetId]);
				await tx.table("coco_cashu_counters").add({
					...counter,
					mintUrl: normalized
				});
			}
			const proofs = await tx.table("coco_cashu_proofs").where("mintUrl").equals(original).toArray();
			for (const proof of proofs) {
				await tx.table("coco_cashu_proofs").delete([original, proof.secret]);
				await tx.table("coco_cashu_proofs").add({
					...proof,
					mintUrl: normalized
				});
			}
			const mintQuotes = await tx.table("coco_cashu_mint_quotes").where("mintUrl").equals(original).toArray();
			for (const quote of mintQuotes) {
				await tx.table("coco_cashu_mint_quotes").delete([original, quote.quote]);
				await tx.table("coco_cashu_mint_quotes").add({
					...quote,
					mintUrl: normalized
				});
			}
			const meltQuotes = await tx.table("coco_cashu_melt_quotes").where("mintUrl").equals(original).toArray();
			for (const quote of meltQuotes) {
				await tx.table("coco_cashu_melt_quotes").delete([original, quote.quote]);
				await tx.table("coco_cashu_melt_quotes").add({
					...quote,
					mintUrl: normalized
				});
			}
			await tx.table("coco_cashu_history").where("mintUrl").equals(original).modify({ mintUrl: normalized });
		}
	});
}

//#endregion
//#region src/repositories/MintRepository.ts
var IdbMintRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async isTrustedMint(mintUrl) {
		return (await this.db.table("coco_cashu_mints").get(mintUrl))?.trusted ?? false;
	}
	async getMintByUrl(mintUrl) {
		const row = await this.db.table("coco_cashu_mints").get(mintUrl);
		if (!row) throw new Error(`Mint not found: ${mintUrl}`);
		return {
			mintUrl: row.mintUrl,
			name: row.name,
			mintInfo: JSON.parse(row.mintInfo),
			trusted: row.trusted ?? true,
			createdAt: row.createdAt,
			updatedAt: row.updatedAt
		};
	}
	async getAllMints() {
		return (await this.db.table("coco_cashu_mints").toArray()).map((r) => ({
			mintUrl: r.mintUrl,
			name: r.name,
			mintInfo: JSON.parse(r.mintInfo),
			trusted: r.trusted ?? true,
			createdAt: r.createdAt,
			updatedAt: r.updatedAt
		}));
	}
	async getAllTrustedMints() {
		return (await this.db.table("coco_cashu_mints").toArray()).filter((r) => r.trusted ?? true).map((r) => ({
			mintUrl: r.mintUrl,
			name: r.name,
			mintInfo: JSON.parse(r.mintInfo),
			trusted: r.trusted ?? true,
			createdAt: r.createdAt,
			updatedAt: r.updatedAt
		}));
	}
	async addNewMint(mint) {
		const row = {
			mintUrl: mint.mintUrl,
			name: mint.name,
			mintInfo: JSON.stringify(mint.mintInfo),
			trusted: mint.trusted,
			createdAt: mint.createdAt,
			updatedAt: mint.updatedAt
		};
		await this.db.table("coco_cashu_mints").put(row);
	}
	async addOrUpdateMint(mint) {
		const existing = await this.db.table("coco_cashu_mints").get(mint.mintUrl);
		const row = {
			mintUrl: mint.mintUrl,
			name: mint.name,
			mintInfo: JSON.stringify(mint.mintInfo),
			trusted: mint.trusted,
			createdAt: existing?.createdAt ?? mint.createdAt,
			updatedAt: mint.updatedAt
		};
		await this.db.table("coco_cashu_mints").put(row);
	}
	async updateMint(mint) {
		await this.addNewMint(mint);
	}
	async setMintTrusted(mintUrl, trusted) {
		await this.db.table("coco_cashu_mints").update(mintUrl, { trusted });
	}
	async deleteMint(mintUrl) {
		await this.db.table("coco_cashu_mints").delete(mintUrl);
	}
};

//#endregion
//#region src/repositories/KeysetRepository.ts
var IdbKeysetRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async getKeysetsByMintUrl(mintUrl) {
		return (await this.db.table("coco_cashu_keysets").where("mintUrl").equals(mintUrl).toArray()).map((r) => ({
			mintUrl: r.mintUrl,
			id: r.id,
			unit: r.unit ?? "",
			keypairs: JSON.parse(r.keypairs),
			active: !!r.active,
			feePpk: r.feePpk,
			updatedAt: r.updatedAt
		}));
	}
	async getKeysetById(mintUrl, id) {
		const row = await this.db.table("coco_cashu_keysets").get([mintUrl, id]);
		if (!row) return null;
		return {
			mintUrl: row.mintUrl,
			id: row.id,
			unit: row.unit ?? "",
			keypairs: JSON.parse(row.keypairs),
			active: !!row.active,
			feePpk: row.feePpk,
			updatedAt: row.updatedAt
		};
	}
	async updateKeyset(keyset) {
		const existing = await this.db.table("coco_cashu_keysets").get([keyset.mintUrl, keyset.id]);
		const now = Math.floor(Date.now() / 1e3);
		if (!existing) {
			await this.db.table("coco_cashu_keysets").put({
				mintUrl: keyset.mintUrl,
				id: keyset.id,
				unit: keyset.unit,
				keypairs: JSON.stringify({}),
				active: keyset.active ? 1 : 0,
				feePpk: keyset.feePpk,
				updatedAt: now
			});
			return;
		}
		await this.db.table("coco_cashu_keysets").put({
			...existing,
			unit: keyset.unit,
			active: keyset.active ? 1 : 0,
			feePpk: keyset.feePpk,
			updatedAt: now
		});
	}
	async addKeyset(keyset) {
		const now = Math.floor(Date.now() / 1e3);
		const row = {
			mintUrl: keyset.mintUrl,
			id: keyset.id,
			unit: keyset.unit,
			keypairs: JSON.stringify(keyset.keypairs ?? {}),
			active: keyset.active ? 1 : 0,
			feePpk: keyset.feePpk,
			updatedAt: now
		};
		await this.db.table("coco_cashu_keysets").put(row);
	}
	async deleteKeyset(mintUrl, keysetId) {
		await this.db.table("coco_cashu_keysets").delete([mintUrl, keysetId]);
	}
};

//#endregion
//#region src/utils.ts
/**
* Safely converts a hex string to Uint8Array with validation
* @throws Error if the hex string is invalid or malformed
*/
function hexToBytes(hexString) {
	if (!/^[0-9a-fA-F]+$/.test(hexString)) throw new Error(`Invalid hex string: contains non-hex characters`);
	if (hexString.length % 2 !== 0) throw new Error(`Invalid hex string: odd length (${hexString.length})`);
	const matches = hexString.match(/.{2}/g);
	if (!matches) throw new Error(`Failed to parse hex string`);
	return new Uint8Array(matches.map((byte) => parseInt(byte, 16)));
}
/**
* Converts a Uint8Array to hex string
*/
function bytesToHex(bytes) {
	return Array.from(bytes).map((b) => b.toString(16).padStart(2, "0")).join("");
}

//#endregion
//#region src/repositories/KeyRingRepository.ts
var IdbKeyRingRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async getPersistedKeyPair(publicKey) {
		const row = await this.db.table("coco_cashu_keypairs").get(publicKey);
		if (!row) return null;
		const keypairRow = row;
		const secretKeyBytes = hexToBytes(keypairRow.secretKey);
		return {
			publicKeyHex: keypairRow.publicKey,
			secretKey: secretKeyBytes,
			derivationIndex: keypairRow.derivationIndex
		};
	}
	async setPersistedKeyPair(keyPair) {
		const table = this.db.table("coco_cashu_keypairs");
		const secretKeyHex = bytesToHex(keyPair.secretKey);
		await table.put({
			publicKey: keyPair.publicKeyHex,
			secretKey: secretKeyHex,
			createdAt: Date.now(),
			derivationIndex: keyPair.derivationIndex
		});
	}
	async deletePersistedKeyPair(publicKey) {
		await this.db.table("coco_cashu_keypairs").delete(publicKey);
	}
	async getAllPersistedKeyPairs() {
		return (await this.db.table("coco_cashu_keypairs").toArray()).map((row) => ({
			publicKeyHex: row.publicKey,
			secretKey: hexToBytes(row.secretKey),
			derivationIndex: row.derivationIndex
		}));
	}
	async getLatestKeyPair() {
		const row = await this.db.table("coco_cashu_keypairs").orderBy("createdAt").reverse().first();
		if (!row) return null;
		return {
			publicKeyHex: row.publicKey,
			secretKey: hexToBytes(row.secretKey),
			derivationIndex: row.derivationIndex
		};
	}
	async getLastDerivationIndex() {
		const row = await this.db.table("coco_cashu_keypairs").orderBy("derivationIndex").reverse().first();
		if (!row || row.derivationIndex == null) return -1;
		return row.derivationIndex;
	}
};

//#endregion
//#region src/repositories/CounterRepository.ts
var IdbCounterRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async getCounter(mintUrl, keysetId) {
		const row = await this.db.table("coco_cashu_counters").get([mintUrl, keysetId]);
		if (!row) return null;
		return {
			mintUrl,
			keysetId,
			counter: row.counter
		};
	}
	async setCounter(mintUrl, keysetId, counter) {
		await this.db.table("coco_cashu_counters").put({
			mintUrl,
			keysetId,
			counter
		});
	}
};

//#endregion
//#region src/repositories/ProofRepository.ts
var IdbProofRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async saveProofs(mintUrl, proofs) {
		if (!proofs || proofs.length === 0) return;
		const now = Math.floor(Date.now() / 1e3);
		await this.db.runTransaction("rw", ["coco_cashu_proofs"], async (tx) => {
			const table = tx.table("coco_cashu_proofs");
			for (const p of proofs) if (await table.get([mintUrl, p.secret])) throw new Error(`Proof with secret already exists: ${p.secret}`);
			for (const p of proofs) {
				const row = {
					mintUrl,
					id: p.id,
					amount: p.amount,
					secret: p.secret,
					C: p.C,
					dleqJson: p.dleq ? JSON.stringify(p.dleq) : null,
					witness: p.witness ? JSON.stringify(p.witness) : null,
					state: p.state,
					createdAt: now
				};
				await table.put(row);
			}
		});
	}
	async getReadyProofs(mintUrl) {
		return (await this.db.table("coco_cashu_proofs").where("[mintUrl+state]").equals([mintUrl, "ready"]).toArray()).map((r) => {
			return {
				id: r.id,
				amount: r.amount,
				secret: r.secret,
				C: r.C,
				...r.dleqJson ? { dleq: JSON.parse(r.dleqJson) } : {},
				...r.witness ? { witness: JSON.parse(r.witness) } : {},
				mintUrl,
				state: "ready"
			};
		});
	}
	async getAllReadyProofs() {
		return (await this.db.table("coco_cashu_proofs").where("state").equals("ready").toArray()).map((r) => {
			return {
				id: r.id,
				amount: r.amount,
				secret: r.secret,
				C: r.C,
				...r.dleqJson ? { dleq: JSON.parse(r.dleqJson) } : {},
				...r.witness ? { witness: JSON.parse(r.witness) } : {},
				mintUrl: r.mintUrl,
				state: "ready"
			};
		});
	}
	async getProofsByKeysetId(mintUrl, keysetId) {
		return (await this.db.table("coco_cashu_proofs").where("[mintUrl+id+state]").equals([
			mintUrl,
			keysetId,
			"ready"
		]).toArray()).map((r) => {
			return {
				id: r.id,
				amount: r.amount,
				secret: r.secret,
				C: r.C,
				...r.dleqJson ? { dleq: JSON.parse(r.dleqJson) } : {},
				...r.witness ? { witness: JSON.parse(r.witness) } : {},
				mintUrl,
				state: "ready"
			};
		});
	}
	async setProofState(mintUrl, secrets, state) {
		if (!secrets || secrets.length === 0) return;
		await this.db.runTransaction("rw", ["coco_cashu_proofs"], async (tx) => {
			const table = tx.table("coco_cashu_proofs");
			for (const s of secrets) {
				const existing = await table.get([mintUrl, s]);
				if (existing) await table.put({
					...existing,
					state
				});
			}
		});
	}
	async deleteProofs(mintUrl, secrets) {
		if (!secrets || secrets.length === 0) return;
		await this.db.runTransaction("rw", ["coco_cashu_proofs"], async (tx) => {
			const table = tx.table("coco_cashu_proofs");
			for (const s of secrets) await table.delete([mintUrl, s]);
		});
	}
	async wipeProofsByKeysetId(mintUrl, keysetId) {
		await this.db.runTransaction("rw", ["coco_cashu_proofs"], async (tx) => {
			const table = tx.table("coco_cashu_proofs");
			const rows = await table.where("[mintUrl+id]").equals([mintUrl, keysetId]).toArray();
			for (const r of rows) await table.delete([mintUrl, r.secret]);
		});
	}
};

//#endregion
//#region src/repositories/MintQuoteRepository.ts
var IdbMintQuoteRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async getMintQuote(mintUrl, quoteId) {
		const row = await this.db.table("coco_cashu_mint_quotes").get([mintUrl, quoteId]);
		if (!row) return null;
		return {
			mintUrl: row.mintUrl,
			quote: row.quote,
			state: row.state,
			request: row.request,
			amount: row.amount,
			unit: row.unit,
			expiry: row.expiry,
			pubkey: row.pubkey ?? void 0
		};
	}
	async addMintQuote(quote) {
		const row = {
			mintUrl: quote.mintUrl,
			quote: quote.quote,
			state: quote.state,
			request: quote.request,
			amount: quote.amount,
			unit: quote.unit,
			expiry: quote.expiry,
			pubkey: quote.pubkey ?? null
		};
		await this.db.table("coco_cashu_mint_quotes").put(row);
	}
	async setMintQuoteState(mintUrl, quoteId, state) {
		const existing = await this.db.table("coco_cashu_mint_quotes").get([mintUrl, quoteId]);
		if (!existing) return;
		await this.db.table("coco_cashu_mint_quotes").put({
			...existing,
			state
		});
	}
	async getPendingMintQuotes() {
		return (await this.db.table("coco_cashu_mint_quotes").toArray()).filter((r) => r.state !== "ISSUED").map((row) => ({
			mintUrl: row.mintUrl,
			quote: row.quote,
			state: row.state,
			request: row.request,
			amount: row.amount,
			unit: row.unit,
			expiry: row.expiry,
			pubkey: row.pubkey ?? void 0
		}));
	}
};

//#endregion
//#region src/repositories/MeltQuoteRepository.ts
var IdbMeltQuoteRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async getMeltQuote(mintUrl, quoteId) {
		const row = await this.db.table("coco_cashu_melt_quotes").get([mintUrl, quoteId]);
		if (!row) return null;
		return {
			mintUrl: row.mintUrl,
			quote: row.quote,
			state: row.state,
			request: row.request,
			amount: row.amount,
			unit: row.unit,
			expiry: row.expiry,
			fee_reserve: row.fee_reserve,
			payment_preimage: row.payment_preimage
		};
	}
	async addMeltQuote(quote) {
		const row = {
			mintUrl: quote.mintUrl,
			quote: quote.quote,
			state: quote.state,
			request: quote.request,
			amount: quote.amount,
			unit: quote.unit,
			expiry: quote.expiry,
			fee_reserve: quote.fee_reserve,
			payment_preimage: quote.payment_preimage ?? null
		};
		await this.db.table("coco_cashu_melt_quotes").put(row);
	}
	async setMeltQuoteState(mintUrl, quoteId, state) {
		const existing = await this.db.table("coco_cashu_melt_quotes").get([mintUrl, quoteId]);
		if (!existing) return;
		await this.db.table("coco_cashu_melt_quotes").put({
			...existing,
			state
		});
	}
	async getPendingMeltQuotes() {
		return (await this.db.table("coco_cashu_melt_quotes").toArray()).filter((r) => r.state !== "PAID").map((row) => ({
			mintUrl: row.mintUrl,
			quote: row.quote,
			state: row.state,
			request: row.request,
			amount: row.amount,
			unit: row.unit,
			expiry: row.expiry,
			fee_reserve: row.fee_reserve,
			payment_preimage: row.payment_preimage
		}));
	}
};

//#endregion
//#region src/repositories/HistoryRepository.ts
var IdbHistoryRepository = class {
	db;
	constructor(db) {
		this.db = db;
	}
	async getPaginatedHistoryEntries(limit, offset) {
		return (await this.db.table("coco_cashu_history").orderBy("createdAt").reverse().offset(offset).limit(limit).toArray()).map((r) => this.rowToEntry(r));
	}
	async addHistoryEntry(history) {
		const row = this.entryToRow(history);
		const id = await this.db.table("coco_cashu_history").add(row);
		const stored = await this.db.table("coco_cashu_history").get(id);
		return this.rowToEntry(stored);
	}
	async getMintHistoryEntry(mintUrl, quoteId) {
		const row = await this.db.table("coco_cashu_history").where("[mintUrl+quoteId+type]").equals([
			mintUrl,
			quoteId,
			"mint"
		]).last();
		if (!row) return null;
		const entry = this.rowToEntry(row);
		return entry.type === "mint" ? entry : null;
	}
	async getMeltHistoryEntry(mintUrl, quoteId) {
		const row = await this.db.table("coco_cashu_history").where("[mintUrl+quoteId+type]").equals([
			mintUrl,
			quoteId,
			"melt"
		]).last();
		if (!row) return null;
		const entry = this.rowToEntry(row);
		return entry.type === "melt" ? entry : null;
	}
	async updateHistoryEntry(history) {
		const coll = this.db.table("coco_cashu_history");
		const rows = await coll.where("[mintUrl+quoteId+type]").equals([
			history.mintUrl,
			history.quoteId,
			history.type
		]).toArray();
		if (!rows.length) throw new Error("History entry not found");
		const row = rows[rows.length - 1];
		const updated = {
			...row,
			unit: history.unit,
			amount: history.amount,
			metadata: history.metadata ?? null
		};
		if (history.type === "mint") {
			updated.state = history.state;
			updated.paymentRequest = history.paymentRequest;
		} else updated.state = history.state;
		await coll.update(row.id, updated);
		const fresh = await coll.get(row.id);
		return this.rowToEntry(fresh);
	}
	async deleteHistoryEntry(mintUrl, quoteId) {
		const coll = this.db.table("coco_cashu_history");
		const ids = (await coll.where("[mintUrl+quoteId+type]").between([
			mintUrl,
			quoteId,
			""
		], [
			mintUrl,
			quoteId,
			""
		]).toArray()).map((r) => r.id);
		await coll.bulkDelete(ids);
	}
	entryToRow(history) {
		const base = {
			mintUrl: history.mintUrl,
			type: history.type,
			unit: history.unit,
			amount: history.amount,
			createdAt: history.createdAt,
			metadata: history.metadata ?? null
		};
		if (history.type === "mint") {
			base.quoteId = history.quoteId;
			base.state = history.state;
			base.paymentRequest = history.paymentRequest;
		} else if (history.type === "melt") {
			base.quoteId = history.quoteId;
			base.state = history.state;
		} else if (history.type === "send") base.tokenJson = JSON.stringify(history.token);
		return base;
	}
	rowToEntry(row) {
		const base = {
			id: String(row.id),
			createdAt: row.createdAt,
			mintUrl: row.mintUrl,
			unit: row.unit,
			metadata: row.metadata ?? void 0
		};
		if (row.type === "mint") return {
			...base,
			type: "mint",
			paymentRequest: row.paymentRequest ?? "",
			quoteId: row.quoteId ?? "",
			state: row.state ?? "UNPAID",
			amount: row.amount
		};
		if (row.type === "melt") return {
			...base,
			type: "melt",
			quoteId: row.quoteId ?? "",
			state: row.state ?? "UNPAID",
			amount: row.amount
		};
		if (row.type === "send") return {
			...base,
			type: "send",
			amount: row.amount,
			token: row.tokenJson ? JSON.parse(row.tokenJson) : {}
		};
		return {
			...base,
			type: "receive",
			amount: row.amount
		};
	}
};

//#endregion
//#region src/index.ts
var IndexedDbRepositories = class {
	mintRepository;
	keyRingRepository;
	counterRepository;
	keysetRepository;
	proofRepository;
	mintQuoteRepository;
	meltQuoteRepository;
	historyRepository;
	db;
	constructor(options) {
		this.db = new IdbDb(options);
		this.mintRepository = new IdbMintRepository(this.db);
		this.keyRingRepository = new IdbKeyRingRepository(this.db);
		this.counterRepository = new IdbCounterRepository(this.db);
		this.keysetRepository = new IdbKeysetRepository(this.db);
		this.proofRepository = new IdbProofRepository(this.db);
		this.mintQuoteRepository = new IdbMintQuoteRepository(this.db);
		this.meltQuoteRepository = new IdbMeltQuoteRepository(this.db);
		this.historyRepository = new IdbHistoryRepository(this.db);
	}
	async init() {
		await ensureSchema(this.db);
	}
	async withTransaction(fn) {
		const stores = this.db.tables.map((t) => t.name);
		return this.db.runTransaction("rw", stores, async () => {
			const scopedDb = this.db;
			const scopedRepositories = {
				mintRepository: new IdbMintRepository(scopedDb),
				keyRingRepository: new IdbKeyRingRepository(scopedDb),
				counterRepository: new IdbCounterRepository(scopedDb),
				keysetRepository: new IdbKeysetRepository(scopedDb),
				proofRepository: new IdbProofRepository(scopedDb),
				mintQuoteRepository: new IdbMintQuoteRepository(scopedDb),
				meltQuoteRepository: new IdbMeltQuoteRepository(scopedDb),
				historyRepository: new IdbHistoryRepository(scopedDb)
			};
			return fn(scopedRepositories);
		});
	}
};

//#endregion
exports.IdbCounterRepository = IdbCounterRepository;
exports.IdbDb = IdbDb;
exports.IdbHistoryRepository = IdbHistoryRepository;
exports.IdbKeyRingRepository = IdbKeyRingRepository;
exports.IdbKeysetRepository = IdbKeysetRepository;
exports.IdbMeltQuoteRepository = IdbMeltQuoteRepository;
exports.IdbMintQuoteRepository = IdbMintQuoteRepository;
exports.IdbMintRepository = IdbMintRepository;
exports.IdbProofRepository = IdbProofRepository;
exports.IndexedDbRepositories = IndexedDbRepositories;
exports.ensureSchema = ensureSchema;