<?php
namespace Acme\Pay\Plugin;

class Log
{
    public function afterPing($subject, bool $result): bool
    {
        return $result;
    }

    // send() is the library parent's TRAIT method.
    public function afterSend($subject, $result)
    {
        return $result;
    }
}
