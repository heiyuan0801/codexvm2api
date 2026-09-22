-- 显式删除意图保留到 Docker 资源清理成功，防止失败后丢失重试依据。
alter table openai_account_slots add column delete_requested_at timestamptz;
alter table openai_account_slots add constraint slot_deletion_requires_unbound_stop
  check (delete_requested_at is null or (not enabled and account_id is null));
