export { setupRxFerrix, getRxFerrixClient } from "./context.js";
export { useQuery, type UseQueryResult, type UseQueryOptions, type ArgsThunk } from "./use_query.svelte.js";
export {
  usePaginatedQuery,
  type UsePaginatedQueryResult,
  type UsePaginatedQueryOptions,
} from "./use_paginated_query.svelte.js";
export { useMutation, useAction, type Mutation, type Action } from "./use_mutation.js";
export { useConnectionState } from "./connection.js";
export { useAuth, useCurrentUser, type UseAuthResult } from "./auth.js";
export {
  RxFerrixClient,
  RxFerrixError,
  RxFerrixAuth,
  getAuth,
  systemApi,
  MemoryAuthStorage,
  type AuthState,
  type AuthStorage,
  type AuthTokens,
  type AuthProviders,
  type CodeProof,
  type RxFerrixAuthOptions,
  type RedirectResult,
  type SignInResult,
  type MfaStatus,
  type MfaSetup,
  type User,
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
  uploadFile,
  type UploadFileOptions,
  type UploadProgress,
} from "@rxferrix/client";
