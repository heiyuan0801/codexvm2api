-- 自动探测代理位置后，把绑定槽位的时区同步到稳定软件身份。
-- 身份改变必须推进代次，确保运行中的容器在下一轮对账时重建环境文件。
create function sync_openai_slot_timezone_from_proxy() returns trigger language plpgsql as $$
begin
  if new.location_timezone is null
     or new.location_timezone is not distinct from old.location_timezone then
    return new;
  end if;

  update openai_account_slots
     set identity_json = jsonb_set(
           identity_json,
           '{timezone}',
           to_jsonb(new.location_timezone),
           true
         ),
         desired_generation = desired_generation + 1,
         updated_at = greatest(now(), updated_at)
   where proxy_id = new.id
     and identity_json->>'timezone' is distinct from new.location_timezone;
  return new;
end;
$$;

create trigger outbound_proxy_slot_timezone
after update of location_timezone on outbound_proxies
for each row execute function sync_openai_slot_timezone_from_proxy();
