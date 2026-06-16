import { SocialGraph } from "../../../src"
import throttle from "lodash/throttle"
import localForage from "localforage"

export const LOCALSTORAGE_PUBLICKEY = "iris.search.currentUser"
export const DEFAULT_SOCIAL_GRAPH_ROOT =
  "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0"
const LOCALFORAGE_KEY = "iris.socialGraph"

let publicKey = DEFAULT_SOCIAL_GRAPH_ROOT
try {
  const k = localStorage.getItem(LOCALSTORAGE_PUBLICKEY)
  if (k) {
    publicKey = k
  }
} catch (e) {
  //
}

let graph: SocialGraph

export const saveGraph = throttle(async () => {
  try {
    const data = await graph.toBinary()
    await localForage.setItem(LOCALFORAGE_KEY, data)
  } catch (e) {
    console.error("Error saving graph", e)
  }
}, 5000)

const initGraph = async () => {
  try {
    const data = await localForage.getItem(LOCALFORAGE_KEY)
    if (data instanceof Uint8Array) {
      graph = await SocialGraph.fromBinary(publicKey, data)
    }
  } catch (e) {
    console.error('Error loading graph')
    localForage.removeItem(LOCALFORAGE_KEY)
  }
  if (!graph) {
    try {
      const { default: socialGraphBinaryUrl } = await import("../../../data/socialGraph.bin?url")
      const response = await fetch(socialGraphBinaryUrl)
      const binaryData = new Uint8Array(await response.arrayBuffer())
      graph = await SocialGraph.fromBinary(publicKey, binaryData)
    } catch (e) {
      console.error('Failed to load social graph binary, creating new graph:', e)
      graph = new SocialGraph(publicKey)
    }
  }
}

export const saveToFile = async () => {
  const data = await graph.toBinary()
  const url = URL.createObjectURL(
    new File([data], "social_graph.bin", {
      type: "text/json",
    }),
  );
  const a = document.createElement("a");
  a.href = url;
  a.download = "social_graph.json";
  a.click();
}

export const loadFromFile = (merge = false) => {
  const input = document.createElement("input")
  input.type = "file"
  input.accept = ".bin"
  input.multiple = false
  input.onchange = () => {
    if (input.files?.length) {
      const file = input.files[0]
      file.arrayBuffer().then(async (buffer) => {
        try {
          const data = new Uint8Array(buffer)
          const newGraph = await SocialGraph.fromBinary(graph.getRoot(), data)
          if (merge) {
            graph.merge(newGraph)
          } else {
            graph = newGraph
          }
        } catch (e) {
          console.error("failed to load social graph from file:", e)
        }
      })
    }
  }
  input.click()
}

export const loadAndMerge = () => loadFromFile(true)

export const downloadLargeGraph = () => {
  fetch("https://drive.iris.to/large_social_graph.bin")
    .then(response => response.arrayBuffer())
    .then(async (buffer) => {
      const data = new Uint8Array(buffer)
      graph = await SocialGraph.fromBinary(graph.getRoot(), data)
      saveGraph()
    })
    .catch(error => {
      console.error("failed to load large social graph:", error)
    })
}

export const socialGraphLoaded = new Promise(async (resolve) => {
  await initGraph()
  resolve(true)
})

export default () => graph
