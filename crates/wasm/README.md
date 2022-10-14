## Wasm WNFS

This projects implements bindings for using [the WebNative FileSystem (WNFS) Rust implementation](../fs) in the browser.

WNFS is a versioned content-addressable distributed filesystem with private and public sub systems. The private filesystem is encrypted so that only users with the right keys can access its contents. It is designed to prevent inferring metadata like the structure of the file tree. The other part of the WNFS filesystem is a simpler public filesystem that is not encrypted and can be accessed by anyone with the right address.

WNFS also features collaborative editing of file trees, where multiple users can edit the same tree at the same time.

WNFS file trees can serialize and be deserialized from IPLD graphs with an extensible metadata section. This allows WNFS to be understood by other IPLD-based tools and systems.

## Outline

- [Setting up the project](#setting-up-the-project)
- [Usage](#usage)
- [Testing the Project](#testing-the-project)
- [Publishing Package](#publishing-package)

## Setting up the Project

- Install `wasm-pack`

  ```bash
  cargo install wasm-pack
  ```

- Install dependencies

  ```bash
  yarn
  ```

- Install playwright binaries

  ```bash
  npx playwright install
  ```

- Build project

  ```bash
  wasm-pack build
  ```

## Usage

WNFS does not have an opinion on where you want to persist your content or the file tree. Instead, the API expects any object that implements the async [`BlockStore`](https://github.com/wnfs-wg/rs-wnfs/blob/07d026c1ef324597da9ac7897353015dd634af16/crates/wasm/fs/blockstore.rs#L20-L29) interface. This implementation also defers system-level operations to the user; requiring that operations like time and random number generation be passed in from the interface. This makes for a clean wasm interface that works everywhere.

Let's see an example of working with a public directory. Here we are going to use a custom-written memory-based blockstore.

```js
import { MemoryBlockStore } from "<custom>";
import { PublicDirectory } from "wnfs";

const dir = new PublicDirectory(new Date());
const store = new MemoryBlockStore();

var { rootDir } = await dir.mkdir(["pictures", "cats"], new Date(), store);

// Create a sample CIDv1.
const cid = Uint8Array.from([
  1, 112, 18, 32, 195, 196, 115, 62, 200, 175, 253, 6, 207, 158, 159, 245, 15,
  252, 107, 205, 46, 200, 90, 97, 112, 0, 75, 183, 9, 102, 156, 49, 222, 148,
  57, 26,
]);

// Add a file to /pictures/cats.
var { rootDir } = await rootDir.write(
  ["pictures", "cats", "tabby.png"],
  cid,
  time,
  store
);

// Create and add a file to /pictures/dogs directory.
var { rootDir } = await rootDir.write(
  ["pictures", "dogs", "billie.jpeg"],
  cid,
  time,
  store
);

// Delete /pictures/cats directory.
var { rootDir } = await rootDir.rm(["pictures", "cats"], store);

// List all files in /pictures directory.
var { result } = await rootDir.ls(["pictures"], store);

console.log("Files in /pictures directory:", result);
```

You may notice that we use the `rootDir`s returned by each operation in subseqent operations. That is because WNFS internal state is immutable and every operation potentially returns a new root directory. This allows us to track and rollback changes when needed. It also makes collaborative editing easier to implement and reason about. There is a basic demo of the filesystem immutability [here](https://calm-thin-barista.fission.app).

The private filesystem, on the other hand, is a bit more involved. [Hash Array Mapped Trie (HAMT)](https://en.wikipedia.org/wiki/Hash_array_mapped_trie) is used as the intermediate format of private file tree before it is persisted to the blockstore because HAMT helps us hide the hierarchy of the file tree.

```js
import { MemoryBlockStore, Rng } from "<custom>";
import { PrivateDirectory, PrivateForest, Namefilter } from "wnfs";

const initialHamt = new PrivateForest();
const rng = new Rng();
const store = new MemoryBlockStore();
const dir = new PrivateDirectory(new Namefilter(), new Date(), rng);

var { rootDir, hamt } = await root.mkdir(
  ["pictures", "cats"],
  true,
  new Date(),
  initialHamt,
  store,
  rng
);

// Add a file to /pictures/cats.
var { rootDir, hamt } = await rootDir.write(
  ["pictures", "cats", "tabby.png"],
  cid,
  time,
  store
);

// Create and add a file to /pictures/dogs directory.
var { rootDir, hamt } = await rootDir.write(
  ["pictures", "cats", "billie.png"],
  true,
  new Uint8Array([1, 2, 3, 4, 5]),
  new Date(),
  hamt,
  store,
  rng
);

// Delete /pictures/cats directory.
var { rootDir, hamt } = await rootDir.rm(
  ["pictures", "cats"],
  true,
  hamt,
  store,
  rng
);

// List all files in /pictures directory.
var { result } = await rootDir.ls(["pictures"], true, hamt, store);

console.log("Files in /pictures directory:", result);
```

## Testing the Project

- Run tests

  ```bash
  yarn playwright test
  ```

## Publishing Package

- Build the project

  ```bash
  rs-wnfs build --wasm
  ```

- Publish from the `pkg` directory

  ```bash
  cd pkg
  ```

  ```bash
  npm publish
  ```
