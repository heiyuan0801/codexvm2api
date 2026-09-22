-- OpenAI 账号独立容器槽位的期望状态。运行健康由 Host 重建，不写入业务事实表。
create table openai_account_slots (
  account_id text primary key references provider_accounts (id) on delete cascade,
  enabled boolean not null default false,
  instance_id uuid not null unique,
  identity_json jsonb not null,
  desired_generation bigint not null default 1,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  constraint openai_account_slots_identity_ck check (
    jsonb_typeof(identity_json) = 'object'
    and octet_length(identity_json::text) <= 4096
  ),
  constraint openai_account_slots_generation_ck check (desired_generation > 0),
  constraint openai_account_slots_time_ck check (created_at <= updated_at)
);

create index openai_account_slots_enabled_idx
  on openai_account_slots (account_id)
  where enabled;
