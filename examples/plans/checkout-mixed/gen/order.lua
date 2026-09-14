-- The order body the `create-order` call posts.
--
-- A generator rather than a template because the line count varies per order and a
-- template cannot loop. Everything it varies comes from `ctx`: the row the iteration
-- is running as, the product id the search step extracted, and the iteration's own
-- seeded stream -- so two runs of this plan send the same orders.

local function line(rng, index)
  return string.format(
    "<line><sku>SKU-%d</sku><qty>%d</qty></line>",
    rng:int(1000, 9999), rng:int(1, 5))
end

function generate(ctx)
  local lines = {}
  for index = 1, ctx.rng:int(1, 4) do
    lines[#lines + 1] = line(ctx.rng, index)
  end
  return {
    headers = { ["X-Request-Id"] = string.format("order-%d", ctx.iteration) },
    body = string.format(
      "<order><user>%s</user><region>%s</region><product>%s</product><lines>%s</lines></order>",
      ctx.args.user, ctx.rows.users.region, ctx.vars.pid or "none",
      table.concat(lines))
  }
end
