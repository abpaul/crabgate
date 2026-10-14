<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Event update</title></head>
<body style="margin:0;padding:0;background:#F4EFE4;color:#29353C;font-family:Arial,Helvetica,sans-serif;font-size:18px;">
<table role="presentation" cellpadding="0" cellspacing="0" width="100%" style="border-collapse:collapse;background:#F4EFE4;"><tr><td align="center" style="padding:24px 12px;"><table role="presentation" cellpadding="0" cellspacing="0" width="100%" style="max-width:600px;border-collapse:collapse;background:#FFFFFF;border:2px solid #29353C;"><tr><td style="background:#FFC933;height:12px;line-height:12px;font-size:1px;">&nbsp;</td></tr><tr><td style="padding:28px 24px 32px;">
<p style="margin:0 0 20px;font-size:18px;font-weight:900;color:#0B151B;"><img src="{{ logo_url | escape_html }}" width="32" height="32" alt="" style="vertical-align:middle;border:0;margin-right:8px;">CrabGate</p>
<p style="margin:0 0 8px;color:#5F686D;font-size:13px;font-weight:800;text-transform:uppercase;letter-spacing:0.08em;">{{ organizer_name | escape_html }}</p>
<h1 style="margin:0 0 14px;font-size:30px;line-height:1.2;color:#0B151B;">Your event has changed.</h1>
<p style="margin:0 0 20px;line-height:1.5;"><strong>{{ event_title | escape_html }}</strong>, order {{ order_number | escape_html }}</p>
<p style="margin:0 0 8px;font-weight:800;">Previous details</p><p style="margin:0 0 18px;line-height:1.5;">{{ old_when | escape_html }}<br>{{ old_place | escape_html }}</p>
<p style="margin:0 0 8px;font-weight:800;">New details</p><p style="margin:0 0 24px;line-height:1.5;background:#FFDC74;padding:12px;">{{ new_when | escape_html }}<br>{{ new_place | escape_html }}</p>
<p style="margin:0;line-height:1.5;">Your ticket codes haven't changed. If you can't make it, contact the organizer.</p>
</td></tr></table></td></tr></table></body></html>
