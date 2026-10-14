<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>New CrabGate signup</title></head>
<body style="margin:0;padding:24px;background:#F4EFE4;color:#29353C;font-family:Arial,Helvetica,sans-serif;font-size:18px;line-height:1.5;">
<h1 style="margin:0 0 16px;font-size:26px;line-height:1.2;color:#0B151B;">{{ name | escape_html }} signed up</h1>
<p style="margin:0 0 8px;"><strong>Email:</strong> {{ email | escape_html }}</p>
<p style="margin:0 0 8px;"><strong>Shop:</strong> {{ shop_url | escape_html }}</p>
<p style="margin:0 0 16px;"><strong>Next event:</strong> {% if next_event %}{{ next_event | escape_html }}{% else %}not given{% endif %}</p>
<p style="margin:0;">Reply to this email to reach them.</p>
</body></html>
