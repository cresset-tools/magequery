<?php
namespace Acme\Lib;

use Acme\Lib\Traits\SendsRequests;

// A plain composer library (not a Magento module), so it is outside the
// compile scan and reached only through the autoload maps. Its public
// send() comes from a trait.
class Client
{
    use SendsRequests;

    public function ping(): bool
    {
        return true;
    }
}
