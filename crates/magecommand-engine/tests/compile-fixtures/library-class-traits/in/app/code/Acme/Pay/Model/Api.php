<?php
namespace Acme\Pay\Model;

// A module class (scanned) extending a library class (not scanned): its
// interceptor wraps every public method reflection reports, including the
// library parent's trait methods.
class Api extends \Acme\Lib\Client
{
    public function endpoint(): string
    {
        return 'v2';
    }
}
