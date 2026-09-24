-- 槽位独立于账号存在，保留已有资源 UUID 和软件身份。
alter table openai_account_slots drop constraint openai_account_slots_pkey;
alter table openai_account_slots drop constraint openai_account_slots_account_id_fkey;
alter table openai_account_slots alter column account_id drop not null;
alter table openai_account_slots add primary key (instance_id);
alter table openai_account_slots add unique (account_id);
alter table openai_account_slots add foreign key (account_id) references provider_accounts(id) on delete set null;
alter table openai_account_slots add column name text not null default 'OpenAI 容器' check (char_length(name) between 1 and 100);
alter table openai_account_slots add column proxy_id text references outbound_proxies(id) on delete restrict;
alter table openai_account_slots add column start_requested_at timestamptz;

-- 旧版允许直接填写账号代理，为这类地址补齐代理库记录。
insert into outbound_proxies (id, name, proxy_url)
select 'proxy_' || gen_random_uuid()::text, '迁移的槽位代理', a.outbound_proxy_url
from provider_accounts a join openai_account_slots s on s.account_id = a.id
where a.outbound_proxy_url is not null and not exists
(select 1 from outbound_proxies p where p.proxy_url = a.outbound_proxy_url);
update openai_account_slots s set name = left(a.name, 100),
  proxy_id = (select p.id from outbound_proxies p where p.proxy_url = a.outbound_proxy_url order by p.id limit 1),
  start_requested_at = case when s.enabled then now() else null end
from provider_accounts a where a.id = s.account_id;

drop trigger provider_account_slot_proxy_generation on provider_accounts;
drop function advance_openai_slot_proxy_generation();

-- 解绑及账号删除都停止槽位；代次覆盖外键 SET NULL 等非管理接口写入。
create function advance_independent_slot_generation() returns trigger language plpgsql as $$
begin
  if new.account_id is null then new.enabled = false; end if;
  if (new.account_id, new.proxy_id, new.enabled) is distinct from
     (old.account_id, old.proxy_id, old.enabled) and new.desired_generation = old.desired_generation then
    new.desired_generation = old.desired_generation + 1;
  end if;
  new.updated_at = greatest(now(), old.updated_at);
  return new;
end;
$$;
create trigger independent_slot_generation before update on openai_account_slots
for each row execute function advance_independent_slot_generation();

-- 编辑代理库连接信息后重建所有关联槽位，不再从账号代理派生槽位出口。
create function advance_slot_proxy_generation() returns trigger language plpgsql as $$
begin
  if new.proxy_url is distinct from old.proxy_url then
    update openai_account_slots set desired_generation = desired_generation + 1 where proxy_id = new.id;
  end if;
  return new;
end;
$$;
create trigger outbound_proxy_slot_generation after update of proxy_url on outbound_proxies
for each row execute function advance_slot_proxy_generation();
