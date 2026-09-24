-- 所有账号代理写入入口共享代次推进，避免代理库编辑、批量修改或解除绑定漏掉槽位重建。
create function advance_openai_slot_proxy_generation() returns trigger language plpgsql as $$
begin
  if new.outbound_proxy_url is distinct from old.outbound_proxy_url then
    update openai_account_slots
       set desired_generation = desired_generation + 1,
           updated_at = greatest(now(), updated_at)
     where account_id = new.id;
  end if;
  return new;
end;
$$;

create trigger provider_account_slot_proxy_generation
  after update of outbound_proxy_url on provider_accounts
  for each row execute function advance_openai_slot_proxy_generation();
