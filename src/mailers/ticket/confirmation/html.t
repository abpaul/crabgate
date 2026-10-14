<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Your tickets</title></head>
<body style="margin:0;padding:0;background:#F4EFE4;color:#29353C;font-family:Arial,Helvetica,sans-serif;font-size:18px;">
<table role="presentation" cellpadding="0" cellspacing="0" width="100%" style="border-collapse:collapse;background:#F4EFE4;"><tr><td align="center" style="padding:24px 12px;">
<table role="presentation" cellpadding="0" cellspacing="0" width="100%" style="max-width:600px;border-collapse:collapse;background:#FFFFFF;border:2px solid #29353C;"><tr><td style="background:#FFC933;height:12px;line-height:12px;font-size:1px;">&nbsp;</td></tr><tr><td style="padding:28px 24px 32px;">
<p style="margin:0 0 20px;font-size:18px;font-weight:900;color:#0B151B;"><img src="{{ logo_url | escape_html }}" width="32" height="32" alt="" style="vertical-align:middle;border:0;margin-right:8px;">CrabGate</p>
<p style="margin:0 0 8px;color:#5F686D;font-size:13px;font-weight:800;text-transform:uppercase;letter-spacing:0.08em;">{{ organizer_name | escape_html }}</p>
<h1 style="margin:0 0 14px;font-size:30px;line-height:1.2;color:#0B151B;">Your tickets are ready.</h1>
<p style="margin:0 0 20px;line-height:1.5;">Order <strong>{{ order_number | escape_html }}</strong></p>
<p style="margin:0 0 24px;"><a href="{{ order_url | escape_html }}" style="display:inline-block;background:#29353C;color:#FFFFFF;text-decoration:none;font-weight:800;padding:14px 20px;border-radius:4px;border-bottom:4px solid #0B151B;">Open your order</a></p>
{% for ticket in tickets %}
<table role="presentation" cellpadding="0" cellspacing="0" width="100%" style="border-collapse:collapse;border:2px solid #29353C;margin:0 0 18px;"><tr><td style="background:#FFDC74;padding:10px 16px;font-size:14px;font-weight:800;text-transform:uppercase;">Ticket {{ loop.index }}, {{ ticket.ticket_type | escape_html }}</td></tr><tr><td style="padding:18px 16px;">
{% if ticket.attendee_name %}<p style="margin:0 0 12px;line-height:1.5;">{{ ticket.attendee_name | escape_html }}</p>{% endif %}
{% if ticket.seat %}<p style="margin:0 0 12px;line-height:1.5;font-weight:800;">{{ ticket.seat | escape_html }}</p>{% endif %}
<p style="margin:0 0 14px;font:800 22px/1.3 monospace;color:#0B151B;letter-spacing:0.06em;">{{ ticket.code | escape_html }}</p>
<img src="{{ ticket.qr_url | escape_html }}" width="180" height="180" alt="QR code for ticket {{ ticket.code | escape_html }}" style="display:block;max-width:100%;height:auto;">
<p style="margin:12px 0 0;font-size:14px;line-height:1.5;"><a href="{{ ticket.qr_url | escape_html }}" style="color:#29353C;text-decoration:underline;font-weight:700;">Open this ticket's QR code</a></p>
</td></tr></table>
{% endfor %}
<p style="margin:20px 0 16px;line-height:1.5;">Show each ticket code at the door. One code lets one person in.</p>
<p style="margin:0;font-size:14px;line-height:1.5;">Manage order {{ order_number | escape_html }}:<br><a href="{{ order_url | escape_html }}" style="color:#29353C;text-decoration:underline;font-weight:700;word-break:break-all;">{{ order_url | escape_html }}</a></p>
</td></tr><tr><td style="border-top:2px dashed #29353C;padding:18px 24px;color:#5F686D;font-size:14px;">The ticket codes above work even if images don't load.</td></tr></table>
</td></tr></table>
</body></html>
