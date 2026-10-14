Your tickets are ready.

Order {{ order_number }}
Open your order: {{ order_url }}

{% for ticket in tickets %}Ticket {{ loop.index }}: {{ ticket.code }}{% if ticket.seat %}, {{ ticket.seat }}{% endif %}
{% endfor %}

Show each ticket code at the door. One code lets one person in.
