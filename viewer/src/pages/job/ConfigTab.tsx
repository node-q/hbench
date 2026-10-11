import { p, useApi } from "../../lib/api";
import type { Obj } from "../../lib/types";
import { JsonTree } from "../../ui/content";
import { CopyButton, Empty, ErrorBox, SkeletonRows } from "../../ui/kit";
import { ApiError } from "../../lib/api";

export function ConfigTab({ job }: { job: string }) {
  const res = useApi<Obj>(p("jobs", job, "config"));
  if (res.error instanceof ApiError && res.error.status === 404) return <Empty title="No config.json">This job folder has no readable config.</Empty>;
  if (res.error) return <ErrorBox error={res.error} onRetry={res.reload} />;
  if (!res.data) return <SkeletonRows />;
  return (
    <section className="panel">
      <div className="panel__head">
        <span className="label">config.json</span>
        <CopyButton text={JSON.stringify(res.data, null, 2)} label="Copy config JSON" />
      </div>
      <div className="panel__body">
        <JsonTree value={res.data} open={2} />
      </div>
    </section>
  );
}
