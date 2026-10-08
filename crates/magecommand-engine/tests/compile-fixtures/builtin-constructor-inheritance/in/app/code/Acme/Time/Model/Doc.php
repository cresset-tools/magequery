<?php

namespace Acme\Time\Model;

// DOMDocument's constructor is real but unmodelled, so this must degrade to a
// NULL row WITH a compile finding naming it — never a silent empty row.
class Doc extends \DOMDocument
{
}
