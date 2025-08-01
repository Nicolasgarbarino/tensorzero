import {
  type ContentBlockOutput,
  CountSchema,
  type JsonInferenceOutput,
  modelInferenceInputMessageSchema,
  type TableBounds,
  type TableBoundsWithCount,
  TableBoundsWithCountSchema,
} from "./common";
import {
  contentBlockSchema,
  getInferenceTableName,
  inputSchema,
} from "./common";
import { data } from "react-router";
import type { FunctionConfig } from "tensorzero-node";
import { getClickhouseClient } from "./client.server";
import { resolveInput, resolveModelInferenceMessages } from "../resolve.server";
import {
  episodeByIdSchema,
  inferenceByIdRowSchema,
  modelInferenceRowSchema,
  parsedModelInferenceRowSchema,
  parseInferenceOutput,
  adjacentIdsSchema,
  type AdjacentIds,
  type EpisodeByIdRow,
  type InferenceByIdRow,
  type InferenceRow,
  type ModelInferenceRow,
  type ParsedInferenceRow,
  type ParsedModelInferenceRow,
} from "./inference";
import { z } from "zod";
import { logger } from "~/utils/logger";
import { getConfig, getFunctionConfig } from "../config/index.server";

/**
 * Query a table of at most `page_size` Inferences from ChatInference or JsonInference that are
 * before the given `before` ID or after the given `after` ID. If `episode_id` is provided,
 * we only return rows from that specific episode.
 *
 * - If `before` and `after` are both not provided, returns the most recent `page_size` Inferences.
 * - If `before` and `after` are both provided, throw an error.
 * - If `before` is provided, returns the most recent `page_size` Inferences before the given `before` ID.
 * - If `after` is provided, returns the earliest `page_size` Inferences after the given `after` ID.
 *
 * All returned data should be ordered by `id` in descending order.
 *
 * TODO (#2788): Create MVs for sorting episodes and inferences by ID DESC
 */
export async function queryInferenceTable(params: {
  page_size: number;
  before?: string; // UUIDv7 string
  after?: string; // UUIDv7 string
  /**
   * Extra WHERE clauses, e.g. ["episode_id = {episode_id:UUID}", "variant_name = {variant:String}"]
   * Use param placeholders if you want to avoid manual string interpolation.
   */
  extraWhere?: string[];
  /**
   * Extra query parameters, mapping placeholders (like "episode_id") => actual values
   */
  extraParams?: Record<string, string | number>;
}): Promise<InferenceByIdRow[]> {
  const { page_size, before, after, extraWhere, extraParams } = params;

  if (before && after) {
    throw new Error("Cannot specify both 'before' and 'after' parameters");
  }

  // We'll build up WHERE clauses incrementally
  const whereClauses: string[] = [];

  // Base query params
  const query_params: Record<string, string | number> = {
    page_size,
  };

  // Add the built-in before/after logic
  if (before) {
    whereClauses.push("id_uint < toUInt128(toUUID({before:String}))");
    query_params.before = before;
  }
  if (after) {
    whereClauses.push("id_uint > toUInt128(toUUID({after:String}))");
    query_params.after = after;
  }

  // Merge in caller-supplied where clauses
  if (extraWhere && extraWhere.length) {
    whereClauses.push(...extraWhere);
  }

  // Merge in caller-supplied params
  if (extraParams) {
    Object.entries(extraParams).forEach(([key, value]) => {
      query_params[key] = value;
    });
  }

  // We'll build the actual WHERE portion here (if any).
  const combinedWhere = whereClauses.length
    ? `WHERE ${whereClauses.join(" AND ")}`
    : "";

  let query: string;
  if (!before && !after) {
    // No "before"/"after" => get the most recent page_size items
    query = `
      SELECT
        uint_to_uuid(id_uint) as id,
        function_name,
        variant_name,
        episode_id,
        function_type,
        formatDateTime(UUIDv7ToDateTime(uint_to_uuid(id_uint)), '%Y-%m-%dT%H:%i:%SZ') AS timestamp
      FROM InferenceById FINAL
      ${combinedWhere}
      ORDER BY id_uint DESC
      LIMIT {page_size:UInt32}
    `;
  } else if (before) {
    // "Most recent" page_size before given ID
    query = `
      SELECT
        uint_to_uuid(id_uint) as id,
        function_name,
        variant_name,
        episode_id,
        function_type,
        formatDateTime(UUIDv7ToDateTime(uint_to_uuid(id_uint)), '%Y-%m-%dT%H:%i:%SZ') AS timestamp
      FROM InferenceById FINAL
      ${combinedWhere}
      ORDER BY id_uint DESC
      LIMIT {page_size:UInt32}
    `;
  } else {
    // "Earliest" page_size after given ID => subselect ascending, then reorder descending
    query = `
      SELECT
        id,
        function_name,
        variant_name,
        episode_id,
        function_type,
        formatDateTime(UUIDv7ToDateTime(id), '%Y-%m-%dT%H:%i:%SZ') AS timestamp
      FROM
      (
        SELECT
          uint_to_uuid(id_uint) as id,
          id_uint,
          function_name,
          variant_name,
          episode_id,
          function_type,
          formatDateTime(UUIDv7ToDateTime(uint_to_uuid(id_uint)), '%Y-%m-%dT%H:%i:%SZ') AS timestamp
        FROM InferenceById FINAL
        ${combinedWhere}
        ORDER BY id_uint ASC
        LIMIT {page_size:UInt32}
      )
      ORDER BY id_uint DESC
    `;
  }

  try {
    const resultSet = await getClickhouseClient().query({
      query,
      format: "JSONEachRow",
      query_params,
    });
    const rows = await resultSet.json<InferenceByIdRow>();
    return z.array(inferenceByIdRowSchema).parse(rows);
  } catch (error) {
    logger.error(error);
    throw data("Error querying inference table", { status: 500 });
  }
}

/// TODO (#2788): Create MVs for sorting episodes and inferences by ID DESC
export async function queryInferenceTableBounds(params?: {
  extraWhere?: string[];
  extraParams?: Record<string, string | number>;
}): Promise<TableBoundsWithCount> {
  const { extraWhere = [], extraParams = {} } = params ?? {};

  // Build WHERE clause
  const whereClauses = [...extraWhere];
  const whereClause =
    whereClauses.length > 0 ? `WHERE ${whereClauses.join(" AND ")}` : "";

  // IMPORTANT: This query will return zero UUIDs if there are no results, so we need to handle this case below.
  const query = `
  SELECT
    uint_to_uuid(MIN(id_uint)) AS first_id,
    uint_to_uuid(MAX(id_uint)) AS last_id,
    toUInt32(COUNT()) AS count
  FROM InferenceById FINAL
  ${whereClause}
  LIMIT 1
  `;

  try {
    const resultSet = await getClickhouseClient().query({
      query,
      format: "JSONEachRow",
      query_params: extraParams,
    });
    const rows = await resultSet.json<TableBoundsWithCount>();

    // Handle the case where there are no results
    if (!rows.length || rows[0].count === 0) {
      return {
        first_id: null,
        last_id: null,
        count: 0,
      };
    }

    return TableBoundsWithCountSchema.parse(rows[0]);
  } catch (error) {
    logger.error("Failed to query inference table bounds:", error);
    throw data("Error querying inference table bounds", { status: 500 });
  }
}

/// Query a table of at most `page_size` episodes that are before the given `before` ID or after the given `after` ID.
/// If neither `before` nor `after` are provided, the query will return the most recent `page_size` episodes.
/// If `before` and `after` are both provided, we will throw an error.
/// If `before` is provided, the query will return the most recent `page_size` episodes before the given `before` ID.
/// If `after` is provided, the query will return the earliest `page_size` episodes after the given `after` ID.
///
/// All returned data should be ordered by `episode_id_uint` in descending order.
///
/// TODO (#2788): Create MVs for sorting episodes and inferences by ID DESC
export async function queryEpisodeTable(params: {
  page_size: number;
  before?: string; // UUIDv7 string
  after?: string; // UUIDv7 string
}): Promise<EpisodeByIdRow[]> {
  const { page_size, before, after } = params;
  if (before && after) {
    throw new Error("Cannot specify both `before` and `after` parameters");
  }
  let query = "";
  const query_params: Record<string, string | number> = {
    page_size,
  };
  if (!before && !after) {
    query = `
      WITH latest_episode_ids_uint AS (
        SELECT DISTINCT episode_id_uint
        FROM InferenceByEpisodeId FINAL
        -- Filter out episodes in the future i.e. dynamic evaluations
        WHERE episode_id_uint <= (SELECT toUInt128(generateUUIDv7()) AS uuid_now)
        ORDER BY episode_id_uint DESC
        LIMIT {page_size:UInt32}
      )
      SELECT
        uint_to_uuid(episode_id_uint) as episode_id,
        toUInt32(count(*)) as count,
        formatDateTime(min(UUIDv7ToDateTime(uint_to_uuid(id_uint))), '%Y-%m-%dT%H:%i:%SZ') as start_time,
        formatDateTime(max(UUIDv7ToDateTime(uint_to_uuid(id_uint))), '%Y-%m-%dT%H:%i:%SZ') as end_time,
        uint_to_uuid(max(id_uint)) as last_inference_id
      FROM InferenceByEpisodeId FINAL
      WHERE episode_id_uint IN latest_episode_ids_uint
      GROUP BY episode_id_uint
      ORDER BY episode_id_uint DESC
    `;
  } else if (before) {
    query = `
      WITH latest_episode_ids_uint AS (
        SELECT DISTINCT episode_id_uint
        FROM InferenceByEpisodeId FINAL
        -- Filter out episodes in the future i.e. dynamic evaluations
        WHERE episode_id_uint <= (SELECT toUInt128(generateUUIDv7()) AS uuid_now)
          AND episode_id_uint < toUInt128(toUUID({before:String}))
        ORDER BY episode_id_uint DESC
        LIMIT {page_size:UInt32}
      )
      SELECT
        uint_to_uuid(episode_id_uint) as episode_id,
        toUInt32(count(*)) as count,
        formatDateTime(min(UUIDv7ToDateTime(uint_to_uuid(id_uint))), '%Y-%m-%dT%H:%i:%SZ') as start_time,
        formatDateTime(max(UUIDv7ToDateTime(uint_to_uuid(id_uint))), '%Y-%m-%dT%H:%i:%SZ') as end_time,
        uint_to_uuid(max(id_uint)) as last_inference_id
      FROM InferenceByEpisodeId FINAL
      WHERE episode_id_uint IN latest_episode_ids_uint
      GROUP BY episode_id_uint
      ORDER BY episode_id_uint DESC
    `;
    query_params.before = before;
  } else if (after) {
    query = `
      WITH latest_episode_ids_uint AS (
        SELECT DISTINCT episode_id_uint
        FROM InferenceByEpisodeId FINAL
        -- Filter out episodes in the future i.e. dynamic evaluations
        WHERE episode_id_uint <= (SELECT toUInt128(generateUUIDv7()) AS uuid_now)
          AND episode_id_uint > toUInt128(toUUID({after:String}))
        ORDER BY episode_id_uint ASC
        LIMIT {page_size:UInt32}
      )
      SELECT
        uint_to_uuid(episode_id_uint) as episode_id,
        toUInt32(count(*)) as count,
        formatDateTime(min(UUIDv7ToDateTime(uint_to_uuid(id_uint))), '%Y-%m-%dT%H:%i:%SZ') as start_time,
        formatDateTime(max(UUIDv7ToDateTime(uint_to_uuid(id_uint))), '%Y-%m-%dT%H:%i:%SZ') as end_time,
        uint_to_uuid(max(id_uint)) as last_inference_id
      FROM InferenceByEpisodeId FINAL
      WHERE episode_id_uint IN latest_episode_ids_uint
      GROUP BY episode_id_uint
      ORDER BY episode_id_uint DESC
    `;
    query_params.after = after;
  }

  try {
    const resultSet = await getClickhouseClient().query({
      query,
      format: "JSONEachRow",
      query_params,
    });
    const rows = await resultSet.json<EpisodeByIdRow>();
    const episodeIds = rows.map((episode) => episode.episode_id);
    const uniqueIds = new Set(episodeIds);
    if (uniqueIds.size !== rows.length) {
      logger.warn(
        `Found duplicate episode IDs: ${rows.length - uniqueIds.size} duplicates detected`,
      );
    }
    return z.array(episodeByIdSchema).parse(rows);
  } catch (error) {
    logger.error(error);
    throw data("Error querying episode table", { status: 500 });
  }
}

/// TODO (#2788): Create MVs for sorting episodes and inferences by ID DESC
export async function queryEpisodeTableBounds(): Promise<TableBoundsWithCount> {
  // IMPORTANT: This query will return zero UUIDs if there are no results, so we need to handle this case below.
  const query = `
    SELECT
      uint_to_uuid(MIN(episode_id_uint)) AS first_id,
      uint_to_uuid(MAX(episode_id_uint)) AS last_id,
      toUInt32(COUNT()) AS count
    FROM InferenceByEpisodeId FINAL
    -- Filter out episodes in the future i.e. dynamic evaluations
    WHERE episode_id_uint <= (SELECT toUInt128(generateUUIDv7()) AS uuid_now)
  `;

  try {
    const resultSet = await getClickhouseClient().query({
      query,
      format: "JSONEachRow",
    });
    const rows = await resultSet.json<TableBoundsWithCount>();
    // Handle the case where there are no results
    if (!rows.length || rows[0].count === 0) {
      return {
        first_id: null,
        last_id: null,
        count: 0,
      };
    }
    return TableBoundsWithCountSchema.parse(rows[0]);
  } catch (error) {
    logger.error(error);
    throw data("Error querying episode table bounds", { status: 500 });
  }
}

export async function queryInferenceTableByEpisodeId(params: {
  episode_id: string;
  page_size: number;
  before?: string;
  after?: string;
}): Promise<InferenceByIdRow[]> {
  return queryInferenceTable({
    page_size: params.page_size,
    before: params.before,
    after: params.after,
    extraWhere: ["episode_id = {episode_id:String}"],
    extraParams: { episode_id: params.episode_id },
  });
}

export async function queryInferenceTableBoundsByEpisodeId(params: {
  episode_id: string;
}): Promise<TableBounds> {
  return queryInferenceTableBounds({
    extraWhere: ["episode_id = {episode_id:String}"],
    extraParams: { episode_id: params.episode_id },
  });
}

export async function queryInferenceTableByFunctionName(params: {
  function_name: string;
  page_size: number;
  before?: string;
  after?: string;
}): Promise<InferenceByIdRow[]> {
  return queryInferenceTable({
    page_size: params.page_size,
    before: params.before,
    after: params.after,
    extraWhere: ["function_name = {function_name:String}"],
    extraParams: { function_name: params.function_name },
  });
}

export async function queryInferenceTableBoundsByFunctionName(params: {
  function_name: string;
}): Promise<TableBounds> {
  return queryInferenceTableBounds({
    extraWhere: ["function_name = {function_name:String}"],
    extraParams: { function_name: params.function_name },
  });
}

export async function queryInferenceTableByVariantName(params: {
  function_name: string;
  variant_name: string;
  page_size: number;
  before?: string;
  after?: string;
}): Promise<InferenceByIdRow[]> {
  return queryInferenceTable({
    page_size: params.page_size,
    before: params.before,
    after: params.after,
    extraWhere: [
      "function_name = {function_name:String}",
      "variant_name = {variant_name:String}",
    ],
    extraParams: {
      function_name: params.function_name,
      variant_name: params.variant_name,
    },
  });
}

export async function queryInferenceTableBoundsByVariantName(params: {
  function_name: string;
  variant_name: string;
}): Promise<TableBounds> {
  return queryInferenceTableBounds({
    extraWhere: [
      "function_name = {function_name:String}",
      "variant_name = {variant_name:String}",
    ],
    extraParams: {
      function_name: params.function_name,
      variant_name: params.variant_name,
    },
  });
}

export async function countInferencesForFunction(
  function_name: string,
  function_config: FunctionConfig,
): Promise<number> {
  const inference_table_name = getInferenceTableName(function_config);
  const query = `SELECT toUInt32(COUNT()) AS count FROM ${inference_table_name} WHERE function_name = {function_name:String}`;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { function_name },
  });
  const rows = await resultSet.json<{ count: number }>();
  const parsedRows = rows.map((row) => CountSchema.parse(row));
  return parsedRows[0].count;
}

export async function countInferencesForVariant(
  function_name: string,
  function_config: FunctionConfig,
  variant_name: string,
): Promise<number> {
  const inference_table_name = getInferenceTableName(function_config);
  const query = `SELECT toUInt32(COUNT()) AS count FROM ${inference_table_name} WHERE function_name = {function_name:String} AND variant_name = {variant_name:String}`;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { function_name, variant_name },
  });
  const rows = await resultSet.json<{ count: number }>();
  const parsedRows = rows.map((row) => CountSchema.parse(row));
  return parsedRows[0].count;
}

export async function countInferencesForEpisode(
  episode_id: string,
): Promise<number> {
  const query = `SELECT toUInt32(COUNT()) AS count FROM InferenceByEpisodeId FINAL WHERE episode_id_uint = toUInt128(toUUID({episode_id:String}))`;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { episode_id },
  });
  const rows = await resultSet.json<{ count: string }>();
  const parsedRows = rows.map((row) => CountSchema.parse(row));
  return parsedRows[0].count;
}

async function parseInferenceRow(
  row: InferenceRow,
): Promise<ParsedInferenceRow> {
  const input = inputSchema.parse(JSON.parse(row.input));
  const config = await getConfig();
  const functionConfig = await getFunctionConfig(row.function_name, config);
  const resolvedInput = await resolveInput(input, functionConfig);
  const extra_body = row.extra_body ? JSON.parse(row.extra_body) : undefined;
  if (row.function_type === "chat") {
    return {
      ...row,
      input: resolvedInput,
      output: parseInferenceOutput(row.output) as ContentBlockOutput[],
      inference_params: z
        .record(z.string(), z.unknown())
        .parse(JSON.parse(row.inference_params)),
      tool_params:
        row.tool_params === ""
          ? {}
          : z
              .record(z.string(), z.unknown())
              .parse(JSON.parse(row.tool_params)),
      extra_body,
    };
  } else {
    return {
      ...row,
      input: resolvedInput,
      output: parseInferenceOutput(row.output) as JsonInferenceOutput,
      inference_params: z
        .record(z.string(), z.unknown())
        .parse(JSON.parse(row.inference_params)),
      output_schema: z
        .record(z.string(), z.unknown())
        .parse(JSON.parse(row.output_schema)),
      extra_body,
    };
  }
}

export async function queryInferenceById(
  id: string,
): Promise<ParsedInferenceRow | null> {
  const query = `
    WITH inference AS (
        SELECT
            id_uint,
            function_name,
            variant_name,
            episode_id,
            function_type
        FROM InferenceById FINAL
        WHERE id_uint = toUInt128({id:UUID})
        LIMIT 1
    )
    SELECT
        c.id,
        c.function_name,
        c.variant_name,
        c.episode_id,
        c.input,
        c.output,
        c.tool_params,
        c.inference_params,
        c.processing_time_ms,
        NULL AS output_schema, -- Placeholder for JSON column
        formatDateTime(c.timestamp, '%Y-%m-%dT%H:%i:%SZ') AS timestamp,
        c.tags,
        'chat' AS function_type,
        c.extra_body
    FROM ChatInference c
    WHERE
        'chat' = (SELECT function_type FROM inference)
        AND c.function_name IN (SELECT function_name FROM inference)
        AND c.variant_name IN (SELECT variant_name FROM inference)
        AND c.episode_id IN (SELECT episode_id FROM inference)
        AND c.id = {id:UUID}

    UNION ALL

    SELECT
        j.id,
        j.function_name,
        j.variant_name,
        j.episode_id,
        j.input,
        j.output,
        NULL AS tool_params, -- Placeholder for Chat column
        j.inference_params,
        j.processing_time_ms,
        j.output_schema,
        formatDateTime(j.timestamp, '%Y-%m-%dT%H:%i:%SZ') AS timestamp,
        j.tags,
        'json' AS function_type,
        j.extra_body
    FROM JsonInference j
    WHERE
        'json' = (SELECT function_type FROM inference)
        AND j.function_name IN (SELECT function_name FROM inference)
        AND j.variant_name IN (SELECT variant_name FROM inference)
        AND j.episode_id IN (SELECT episode_id FROM inference)
        AND j.id = {id:UUID}
  `;

  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { id },
  });
  const rows = await resultSet.json<InferenceRow>();
  const firstRow = rows[0];
  if (!firstRow) return null;
  const parsedRow = await parseInferenceRow(firstRow);
  return parsedRow;
}

async function parseModelInferenceRow(
  row: ModelInferenceRow,
): Promise<ParsedModelInferenceRow> {
  const parsedMessages = z
    .array(modelInferenceInputMessageSchema)
    .parse(JSON.parse(row.input_messages));
  const resolvedMessages = await resolveModelInferenceMessages(parsedMessages);
  const processedRow = {
    ...row,
    input_messages: resolvedMessages,
    output: z.array(contentBlockSchema).parse(JSON.parse(row.output)),
  };
  return parsedModelInferenceRowSchema.parse(processedRow);
}

export async function queryModelInferencesByInferenceId(
  id: string,
): Promise<ParsedModelInferenceRow[]> {
  const query = `
    SELECT *, formatDateTime(timestamp, '%Y-%m-%dT%H:%i:%SZ') as timestamp FROM ModelInference WHERE inference_id = {id:String}
  `;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { id },
  });
  const rows = await resultSet.json<ModelInferenceRow>();
  const validatedRows = z.array(modelInferenceRowSchema).parse(rows);
  const parsedRows = await Promise.all(
    validatedRows.map(parseModelInferenceRow),
  );
  return parsedRows;
}

const functionCountInfoSchema = z.object({
  function_name: z.string(),
  max_timestamp: z.string().datetime(),
  count: z.number(),
});

export type FunctionCountInfo = z.infer<typeof functionCountInfoSchema>;

export async function countInferencesByFunction(): Promise<
  FunctionCountInfo[]
> {
  const query = `SELECT
        function_name,
        formatDateTime(max(timestamp), '%Y-%m-%dT%H:%i:%SZ') AS max_timestamp,
        toUInt32(count()) AS count
    FROM (
        SELECT function_name, timestamp
        FROM ChatInference
        UNION ALL
        SELECT function_name, timestamp
        FROM JsonInference
    )
    GROUP BY function_name
    ORDER BY max_timestamp DESC`;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
  });
  const rows = await resultSet.json<FunctionCountInfo[]>();
  const validatedRows = z.array(functionCountInfoSchema).parse(rows);
  return validatedRows;
}

export async function countEpisodes(): Promise<number> {
  const query = `SELECT toUInt32(uniqExact(episode_id_uint)) AS count FROM InferenceByEpisodeId`;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
  });
  const rows = await resultSet.json<{ count: number }>();
  const parsedRows = rows.map((row) => CountSchema.parse(row));
  return parsedRows[0].count;
}

export async function getAdjacentInferenceIds(
  currentInferenceId: string,
): Promise<AdjacentIds> {
  // TODO (soon): add the ability to pass filters by some fields
  const query = `
    SELECT
      NULLIF(
        (SELECT uint_to_uuid(max(id_uint)) FROM InferenceById WHERE id_uint < toUInt128({current_inference_id:UUID})),
        toUUID('00000000-0000-0000-0000-000000000000')
      ) as previous_id,
      NULLIF(
        (SELECT uint_to_uuid(min(id_uint)) FROM InferenceById WHERE id_uint > toUInt128({current_inference_id:UUID})),
        toUUID('00000000-0000-0000-0000-000000000000')
      ) as next_id
  `;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { current_inference_id: currentInferenceId },
  });
  const rows = await resultSet.json<AdjacentIds>();
  const parsedRows = rows.map((row) => adjacentIdsSchema.parse(row));
  return parsedRows[0];
}

export async function getAdjacentEpisodeIds(
  currentEpisodeId: string,
): Promise<AdjacentIds> {
  const query = `
    SELECT
      NULLIF(
        (SELECT DISTINCT uint_to_uuid(max(episode_id_uint)) FROM InferenceByEpisodeId WHERE episode_id_uint < toUInt128({current_episode_id:UUID})),
        toUUID('00000000-0000-0000-0000-000000000000')
      ) as previous_id,
      NULLIF(
        (SELECT DISTINCT uint_to_uuid(min(episode_id_uint)) FROM InferenceByEpisodeId WHERE episode_id_uint > toUInt128({current_episode_id:UUID})),
        toUUID('00000000-0000-0000-0000-000000000000')
      ) as next_id
  `;
  const resultSet = await getClickhouseClient().query({
    query,
    format: "JSONEachRow",
    query_params: { current_episode_id: currentEpisodeId },
  });
  const rows = await resultSet.json<AdjacentIds>();
  const parsedRows = rows.map((row) => adjacentIdsSchema.parse(row));
  return parsedRows[0];
}
