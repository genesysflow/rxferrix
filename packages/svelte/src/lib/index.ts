export { setupFerrix, getFerrixClient } from "./context.js";
export { useQuery, type UseQueryResult, type UseQueryOptions, type ArgsThunk } from "./use_query.svelte.js";
export {
  usePaginatedQuery,
  type UsePaginatedQueryResult,
  type UsePaginatedQueryOptions,
} from "./use_paginated_query.svelte.js";
export { useMutation, useAction, type Mutation, type Action } from "./use_mutation.js";
export { useConnectionState } from "./connection.js";
export {
  FerrixClient,
  FerrixError,
  makeFunctionReference,
  type FunctionReference,
  type ArgsOf,
  type OutputOf,
  type Id,
  type OptimisticLocalStore,
  type OptimisticUpdate,
  type ConnectionState,
  type PaginationOpts,
  type PaginationResult,
  type PaginationStatus,
  insertAtTop,
  getAllPages,
} from "@ferrix/client";
