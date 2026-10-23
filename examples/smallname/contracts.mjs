// What smallname indexes on Sepolia: the ENS contracts bigname's Sepolia manifests declare
// (ENSv1 and ENSv2, Ethereum L1), and the events it needs from them. Only what chain state can't
// answer is indexed: label preimages, the name tree, event history, and candidate lists (owners,
// expiries, record keys) that every response re-checks against live state.
import { parseAbi, parseAbiItem, namehash } from "viem";

export const CHAIN_ID = 11155111;
export const ETH_NODE = namehash("eth");

export const V1 = {
  registry: "0x00000000000C2E074eC69A0dFb2997BA6C7d2e1e",
  registryOld: "0x94f523b8261B815b87EFfCf4d18E6aBeF18d6e4b",
  baseRegistrar: "0x57f1887a8BF19b14fC0dF6Fd9B2acc9Af147eA85",
  nameWrapper: "0x0635513f179D50A207757E05759CbD106d7dFcE8",
  reverseRegistrar: "0xA0a1AbcDAe1a2a4A2EF8e9113Ff0e02DD81DC0C6",
  defaultReverseRegistrar: "0x4F382928805ba0e23B30cFB75fC9E848e82DFD47",
};
export const V1_CONTROLLERS = ["0xFED6a969AaA60E4961FCD3EBF1A2e8913ac65B72", "0x4477cAc137F3353Ca35060E01E5aEb777a1Ca01B"];
// Resolvers whose records are indexed (the declared ones; others' events aren't admitted).
export const V1_RESOLVERS = [
  "0xE99638b40E4Fff0129D56f03b55b6bbC4BBE49b5",
  "0x8948458626811dd0c23EB25Cc74291247077cC51",
  "0x8FADE66B79cC9f707aB26799354482EB93a5B7dD",
  "0x0CeEC524b2807841739D3B5E161F5bf1430FFA48",
];

export const V2 = {
  rootRegistry: "0xb458d6a3a77919449d03e7a6903c26827c1ec43f",
  ethRegistry: "0xd4ebcbbdf463c9c45784603db0ddd499bc44a8b4",
  ethRegistrar: "0xf633e7fc17e2bbe0d0965d18ec1821dcb754a3d3",
  ensv1Resolver: "0x322b7581ca210a69c6d0e0d7c88a7688d2789cb0", // mirrors ENSv1, stores nothing
  publicResolverV2: "0xdc4a563d00f5c3012b699794eb9e13a561be386f",
  renewalBridge: "0xf2ece44980778966b8a0fccb3a9e339440f6e045",
  graveyard: "0xb58a90a39d13cce1d0e192b5da5c47640855b04d",
};
// ENSv2 deployment: registries and resolvers announce themselves after this block.
export const V2_START = 11820288;

export const UNIVERSAL_RESOLVER = "0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe";
// Glamsterdam (Amsterdam EL fork) on Sepolia: the first block with a block access list. skim
// serves history (headers, receipts, logs) from here; smallname's index starts here.
export const AMSTERDAM_BLOCK = 11856337;

/** Contracts whose logs are taken by address (every followed block). */
export const ADDRESSED = [
  V1.registry, V1.registryOld, V1.baseRegistrar, V1.nameWrapper, V1.reverseRegistrar, V1.defaultReverseRegistrar,
  ...V1_CONTROLLERS, ...V1_RESOLVERS,
  V2.rootRegistry, V2.ethRegistry, V2.ethRegistrar, V2.publicResolverV2, V2.renewalBridge,
].map((a) => a.toLowerCase());

export const EVENTS = {
  // ENSv1 registry (both generations)
  NewOwner: parseAbiItem("event NewOwner(bytes32 indexed node, bytes32 indexed label, address owner)"),
  RegistryTransfer: parseAbiItem("event Transfer(bytes32 indexed node, address owner)"),
  NewResolver: parseAbiItem("event NewResolver(bytes32 indexed node, address resolver)"),
  // ENSv1 BaseRegistrar (ERC-721 over labelhash)
  TokenTransfer: parseAbiItem("event Transfer(address indexed from, address indexed to, uint256 indexed tokenId)"),
  RegistrarNameRegistered: parseAbiItem("event NameRegistered(uint256 indexed id, address indexed owner, uint256 expires)"),
  RegistrarNameRenewed: parseAbiItem("event NameRenewed(uint256 indexed id, uint256 expires)"),
  // ENSv1 wrapped controllers: the label preimage for renewals
  ControllerNameRenewed: parseAbiItem("event NameRenewed(string name, bytes32 indexed label, uint256 cost, uint256 expires)"),
  // NameWrapper
  NameWrapped: parseAbiItem("event NameWrapped(bytes32 indexed node, bytes name, address owner, uint32 fuses, uint64 expiry)"),
  NameUnwrapped: parseAbiItem("event NameUnwrapped(bytes32 indexed node, address owner)"),
  FusesSet: parseAbiItem("event FusesSet(bytes32 indexed node, uint32 fuses)"),
  ExpiryExtended: parseAbiItem("event ExpiryExtended(bytes32 indexed node, uint64 expiry)"),
  TransferSingle: parseAbiItem("event TransferSingle(address indexed operator, address indexed from, address indexed to, uint256 id, uint256 value)"),
  TransferBatch: parseAbiItem("event TransferBatch(address indexed operator, address indexed from, address indexed to, uint256[] ids, uint256[] values)"),
  // Node-keyed resolvers (ENSv1 public resolvers, PublicResolverV2)
  AddrChanged: parseAbiItem("event AddrChanged(bytes32 indexed node, address a)"),
  AddressChanged: parseAbiItem("event AddressChanged(bytes32 indexed node, uint256 coinType, bytes newAddress)"),
  TextChanged: parseAbiItem("event TextChanged(bytes32 indexed node, string indexed indexedKey, string key, string value)"),
  TextChangedLegacy: parseAbiItem("event TextChanged(bytes32 indexed node, string indexed indexedKey, string key)"),
  ContenthashChanged: parseAbiItem("event ContenthashChanged(bytes32 indexed node, bytes hash)"),
  ABIChanged: parseAbiItem("event ABIChanged(bytes32 indexed node, uint256 indexed contentType)"),
  NameChanged: parseAbiItem("event NameChanged(bytes32 indexed node, string name)"),
  VersionChanged: parseAbiItem("event VersionChanged(bytes32 indexed node, uint64 newVersion)"),
  // Reverse
  ReverseClaimed: parseAbiItem("event ReverseClaimed(address indexed addr, bytes32 indexed node)"),
  NameForAddrChanged: parseAbiItem("event NameForAddrChanged(address indexed addr, string name)"),
  // ENSv2 registries
  RegistryCreated: parseAbiItem("event RegistryCreated()"),
  LabelRegistered: parseAbiItem("event LabelRegistered(uint256 indexed tokenId, bytes32 indexed labelHash, string label, address owner, uint64 expiry, address indexed sender)"),
  LabelReserved: parseAbiItem("event LabelReserved(uint256 indexed tokenId, bytes32 indexed labelHash, string label, uint64 expiry, address indexed sender)"),
  LabelUnregistered: parseAbiItem("event LabelUnregistered(uint256 indexed tokenId, address indexed sender)"),
  ExpiryUpdated: parseAbiItem("event ExpiryUpdated(uint256 indexed tokenId, uint64 indexed newExpiry, address indexed sender)"),
  SubregistryUpdated: parseAbiItem("event SubregistryUpdated(uint256 indexed tokenId, address indexed subregistry, address indexed sender)"),
  ResolverUpdated: parseAbiItem("event ResolverUpdated(uint256 indexed tokenId, address indexed resolver, address indexed sender)"),
  TokenRegenerated: parseAbiItem("event TokenRegenerated(uint256 indexed oldTokenId, uint256 indexed newTokenId)"),
  TokenResource: parseAbiItem("event TokenResource(uint256 indexed tokenId, uint256 indexed resource)"),
  ParentUpdated: parseAbiItem("event ParentUpdated(address indexed parent, string label, address indexed sender)"),
  EACRolesChanged: parseAbiItem("event EACRolesChanged(uint256 indexed resource, address indexed account, uint256 oldRoleBitmap, uint256 newRoleBitmap)"),
  // ENSv2 registrar / renewal bridge (history and preimages; registry events carry authority)
  V2NameRegistered: parseAbiItem("event NameRegistered(uint256 indexed tokenId, string label, address owner, address subregistry, address resolver, uint64 duration, address paymentToken, bytes32 indexed referrer, uint256 base, uint256 premium)"),
  V2NameRenewed: parseAbiItem("event NameRenewed(uint256 indexed tokenId, string label, uint64 duration, uint64 newExpiry, address paymentToken, bytes32 indexed referrer, uint256 amount)"),
  // ENSv2 record-id resolvers (PermissionedResolver proxies)
  ResolverCreated: parseAbiItem("event ResolverCreated()"),
  Linked: parseAbiItem("event Linked(uint256 indexed recordId, bytes32 indexed node, bytes name)"),
  AddressUpdated: parseAbiItem("event AddressUpdated(uint256 indexed recordId, uint256 coinType, bytes addressBytes)"),
  ContenthashUpdated: parseAbiItem("event ContenthashUpdated(uint256 indexed recordId, bytes hash)"),
  TextUpdated: parseAbiItem("event TextUpdated(uint256 indexed recordId, string indexed keyHash, string key, string value)"),
  NameUpdated: parseAbiItem("event NameUpdated(uint256 indexed recordId, string primaryName)"),
};

/** ENSv2 events taken from any emitter (registries and resolvers announce themselves). */
export const V2_DISCOVERY_EVENTS = [
  "RegistryCreated", "LabelRegistered", "LabelReserved", "LabelUnregistered", "ExpiryUpdated", "SubregistryUpdated",
  "ResolverUpdated", "TokenRegenerated", "TokenResource", "ParentUpdated", "ResolverCreated", "Linked", "AddressUpdated",
  "ContenthashUpdated", "TextUpdated", "NameUpdated",
];

// Live reads (always at a fixed block hash).
export const VIEWS = {
  registry: parseAbi([
    "function owner(bytes32 node) view returns (address)",
    "function resolver(bytes32 node) view returns (address)",
    "function recordExists(bytes32 node) view returns (bool)",
  ]),
  baseRegistrar: parseAbi([
    "function ownerOf(uint256 id) view returns (address)",
    "function nameExpires(uint256 id) view returns (uint256)",
  ]),
  nameWrapper: parseAbi([
    "function getData(uint256 id) view returns (address owner, uint32 fuses, uint64 expiry)",
    "function names(bytes32 node) view returns (bytes)",
  ]),
  v2Registry: parseAbi([
    "function getSubregistry(string label) view returns (address)",
    "function getResolver(string label) view returns (address)",
    "function getState(uint256 anyId) view returns ((uint8 status, uint64 expiry, address latestOwner, uint256 tokenId, uint256 resource))",
    "function ownerOf(uint256 tokenId) view returns (address)",
  ]),
  universalResolver: parseAbi([
    "function resolve(bytes name, bytes data) view returns (bytes, address)",
    "function reverse(bytes lookupAddress, uint256 coinType) view returns (string, address, address)",
  ]),
  resolver: parseAbi([
    "function addr(bytes32 node) view returns (address)",
    "function addr(bytes32 node, uint256 coinType) view returns (bytes)",
    "function text(bytes32 node, string key) view returns (string)",
    "function contenthash(bytes32 node) view returns (bytes)",
    "function name(bytes32 node) view returns (string)",
  ]),
};

// ENSv2 registry entry status (getState).
export const V2_STATUS = { 0: "available", 1: "reserved", 2: "registered" };
export const V1_GRACE_S = 90n * 86400n;
export const V2_GRACE_S = 28n * 86400n;
// NameWrapper fuses
export const PARENT_CANNOT_CONTROL = 1 << 16;
export const CANNOT_UNWRAP = 1;
